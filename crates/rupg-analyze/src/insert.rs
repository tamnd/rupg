//! `transformInsertStmt` of `analyze.c`, with `checkInsertTargets` and `transformAssignedExpr` of `parse_target.c` and the defaults of `rewriteTargetListIU` and `expand_insert_targetlist`.
//!
//! The query of an `INSERT` gives the full row of the table for each new row, in the order of the attributes. A column that the statement does not give gets its default, the default of its domain or a null value. When the table has check constraints, the query reads the rows as a subquery and its targets after the columns are the conditions of the constraints in the order of their names, which the executor tests.

use rupg_catalog::{ConKind, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::{
    InsertStmt, List, LockClauseStrength, Node, OnConflictAction, OnConflictClause, OverridingKind,
    RangeVar, SelectStmt, SortByDir, SortByNulls,
};
use rupg_types::{Value, oid};

use crate::Analyzer;
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::expr::{BoolOp, CastForm, Expr, ExprKind, FieldStore, Func, FuncForm, Var};
use crate::from::{Column, FromItem, Relation, TABLE_OID_ATTNUM};
use crate::merge::Merge;
use crate::modify::{carried_targets, column_var, set_columns};
use crate::perm::{ACL_DELETE, ACL_INSERT, ACL_SELECT, ACL_UPDATE};
use crate::select::{Query, Target};
use crate::typename::place;
use crate::types;

/// The kind of a statement that writes the rows of a table, `CmdType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteKind {
    Insert,
    Update,
    Delete,
    Merge,
}

impl WriteKind {
    /// The name of the statement.
    pub fn name(self) -> &'static str {
        match self {
            WriteKind::Insert => "INSERT",
            WriteKind::Update => "UPDATE",
            WriteKind::Delete => "DELETE",
            WriteKind::Merge => "MERGE",
        }
    }
}

/// The table that an `INSERT`, an `UPDATE` or a `DELETE` writes.
///
/// The rows of the query of an `INSERT` are the new rows, each with the conditions of the check constraints after the columns. The rows of an `UPDATE` are the same, with the ID of the version of the old row at the end. The rows of a `DELETE` have only the ID of the version of the row.
#[derive(Clone, Debug, PartialEq)]
pub struct Write {
    pub kind: WriteKind,
    /// The OID of the table.
    pub table: u32,
    /// The name of the table, for the errors of the constraints.
    pub name: String,
    /// The alias of the target table of an `INSERT`, as `INSERT INTO t AS a`, which `EXPLAIN` shows.
    pub alias: Option<String>,
    /// The number of columns of the table. The first targets of the query are the columns. For a `DELETE` it is 0.
    pub width: usize,
    /// The names of the check constraints in the order of their names. The targets after the columns are the conditions of these constraints, in the same order.
    pub checks: Vec<String>,
    /// The unique indexes of the table in the order of their OIDs, as `RelationGetIndexList` gives them. Each new version of a row must have a key that no other version has.
    pub unique: Vec<Unique>,
    /// The query of `RETURNING`, which reads one relation: the rows that the statement writes. Each row has the columns of the table, then the values that the query of the statement carries for it. For an `UPDATE` and a `DELETE` the carried values come after the columns and the conditions in each row of the query of the statement, before the ID of the version.
    pub returning: Option<Box<Query>>,
    /// The place in each row that `RETURNING` reads of the ID of the new version of an `INSERT` or an `UPDATE`, which `ctid` of the new row reads. The write puts the ID there.
    pub returning_tid: Option<usize>,
    /// The foreign keys that the write checks after the statement, in the order of the names of their triggers.
    pub foreign: Vec<Foreign>,
    /// The `ON CONFLICT` clause of an `INSERT`.
    pub on_conflict: Option<OnConflict>,
    /// The clauses of a `MERGE`.
    pub merge: Option<Merge>,
    /// The query of the checks of the constraints after the triggers `BEFORE ... FOR EACH ROW` of an `INSERT` or an `UPDATE`, which can change the new row. It reads one relation, the new row, and gives its columns, then the conditions of the check constraints. When the write has it, [`Write::checks`] are not checked in the run of the plan: the rows of the run keep the conditions after the columns, and the caller checks the rows after the triggers.
    pub recheck: Option<Box<Query>>,
    /// The numbers of the columns that `SET` of an `UPDATE` or of `ON CONFLICT DO UPDATE` gives, in the order of the list, for the triggers of `UPDATE OF`. For a `MERGE` they are the columns of the `SET` of all its clauses `UPDATE`, as `transformMergeStmt` puts them in `updatedCols`.
    pub updated: Vec<i16>,
    /// True for a write into a view with a trigger `INSTEAD OF ... FOR EACH ROW` for the event. The rows of the query go to the triggers, not to the heap, and the rows of an `UPDATE` and of a `DELETE` end with the old row of the view in place of the ID of the version.
    pub instead: bool,
    /// True for an `UPDATE`, a `DELETE` or a `MERGE` of a table with children of `INHERITS`, with no `ONLY`, as `expand_inherited_rtentry` adds the children. Each row of the query gives the OID of its table before the ID of its version, and each table of the tree checks and changes its rows, as the leaves of a partitioned table do.
    pub inherit: bool,
    /// The checks of `WITH CHECK OPTION` of the views of an `INSERT` or an `UPDATE` of an automatically updatable view.
    pub view_check: Option<ViewCheck>,
    /// The checks of the policies of row-level security of an `INSERT` or an `UPDATE`.
    pub policy_check: Option<PolicyCheck>,
    /// The number of automatically updatable views that the statement writes through. `rewriteTargetView` keeps the entry of each view in the range table and adds the entry of the relation under it.
    pub views: usize,
    /// The numbers of the columns that the statement gives values, as `ExecGetInsertedCols` and `ExecGetUpdatedCols` give them for the detail of a failing row. The plan fills them from the privileges of the query.
    pub modified: Vec<i16>,
    /// The form of an `INSERT` before the rewrite, when the analysis keeps it for a deparse.
    pub insert_def: Option<Box<InsertDef>>,
    /// The items of `SET` of an `UPDATE` before the rewrite, or no items for a `DELETE`, when the analysis keeps the form for a deparse. Each item has the number of its column from 1, and its value, or `None` for `DEFAULT`. The value for a part of a column is the assignment to the part of the old value.
    pub set_def: Option<Vec<(i16, Option<Expr>)>>,
    /// True when the plan has no child table of the target as a result relation: for `ONLY`, and for a `WHERE` of the constant false, which prunes each child. Each other child is a result relation that `CheckValidResultRel` checks.
    pub pruned: bool,
}

/// An `INSERT` as `transformInsertStmt` gives it, before `rewriteTargetListIU` adds the defaults of the other columns, which `get_insert_query_def` shows.
#[derive(Clone, Debug, PartialEq)]
pub struct InsertDef {
    /// The target table, with its alias.
    pub target: Relation,
    /// The numbers of the columns that have a value, from 1, in the order of the column list.
    pub columns: Vec<i16>,
    /// `OVERRIDING SYSTEM VALUE` or `OVERRIDING USER VALUE`.
    pub overriding: OverridingKind,
    pub source: InsertSource,
    /// The clause `ON CONFLICT`.
    pub conflict: Option<ConflictDef>,
}

/// `OnConflictExpr` as `transformOnConflictClause` gives it, which `get_insert_query_def` shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConflictDef {
    /// The elements of the inference, each with its collation, its operator class and the input type of the class, or 0. They read the target table.
    pub elems: Vec<(Expr, u32, u32, u32)>,
    /// The predicate of the inference.
    pub predicate: Option<Expr>,
    /// The OID of the constraint of `ON CONSTRAINT`, or 0.
    pub constraint: u32,
    /// The target table and `EXCLUDED`, which `SET` and `WHERE` of `DO UPDATE` and of `DO SELECT` read.
    pub relations: Vec<Relation>,
    /// The items of `SET` of `DO UPDATE`, as [`Write::set_def`] has them.
    pub set: Option<Vec<(i16, Option<Expr>)>>,
    /// The strength of the lock of `DO SELECT`.
    pub select: Option<LockClauseStrength>,
    /// The condition of `WHERE` of `DO UPDATE` or of `DO SELECT`.
    pub filter: Option<Expr>,
}

/// The rows of an [`InsertDef`].
#[derive(Clone, Debug, PartialEq)]
pub enum InsertSource {
    /// `DEFAULT VALUES`.
    Default,
    /// The single row of `VALUES`, with the value of each column of [`InsertDef::columns`], or `None` for `DEFAULT`.
    Row(Vec<Option<Expr>>),
    /// The rows of `VALUES` with more than one row.
    Values(Vec<Vec<Option<Expr>>>),
    /// The query of `INSERT ... SELECT`, and for each column the assignment to the part of the column of the value of the query, or `None` for a whole column.
    Select(Box<Query>, Vec<Option<Expr>>),
}

impl Write {
    /// The checks of the policies of row-level security on the new rows of the write `kind`. The new rows of `ON CONFLICT DO UPDATE` of an `INSERT` and of an `UPDATE` action of a `MERGE` have the checks of `WCO_RLS_UPDATE_CHECK`.
    #[must_use]
    pub fn policy_for(&self, kind: WriteKind) -> Option<&PolicyCheck> {
        if self.kind == WriteKind::Insert && kind == WriteKind::Update {
            return self.on_conflict.as_ref().and_then(|c| c.update_check.as_ref());
        }
        if self.kind == WriteKind::Merge && kind == WriteKind::Update {
            return self.merge.as_ref().and_then(|m| m.update_check.as_ref());
        }
        self.policy_check.as_ref()
    }

    /// True when the write keeps the rows that it wrote, for `RETURNING` or for [`Write::view_check`].
    #[must_use]
    pub fn keeps_written(&self) -> bool {
        self.returning.is_some() || self.view_check.is_some()
    }
}

/// The `WithCheckOption` items of the kind `WCO_VIEW_CHECK` of a write, which `ExecWithCheckOptions` checks after the write of each row, before `RETURNING`.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewCheck {
    /// The query of the checks. It reads one relation, a row that the statement wrote with the columns of the table, and gives the condition of each view in the order of [`ViewCheck::views`].
    pub query: Box<Query>,
    /// The names of the views, the inner view first, as `rewriteTargetView` puts each new check at the start of the list.
    pub views: Vec<String>,
}

/// The `WithCheckOption` items of the kinds `WCO_RLS_INSERT_CHECK` and `WCO_RLS_UPDATE_CHECK` of a write, which `ExecWithCheckOptions` checks on each new row after the triggers `BEFORE ... FOR EACH ROW` and before the constraints of the table.
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyCheck {
    /// The query of the checks. It reads one relation, a new row with the columns of the table, and gives the condition of each check.
    pub query: Box<Query>,
    /// The name of the policy of each check, or `None` for the check of the permissive policies.
    pub policies: Vec<Option<String>>,
    /// The kind of the checks, which tells the error of a row that does not pass a check.
    pub kind: PolicyKind,
}

/// The kind of the `WithCheckOption` items of a [`PolicyCheck`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyKind {
    /// `WCO_RLS_INSERT_CHECK` and `WCO_RLS_UPDATE_CHECK`: the checks of a new row.
    New,
    /// `WCO_RLS_CONFLICT_CHECK`: the conditions of `USING` on the row that the table has for `ON CONFLICT`.
    Conflict,
    /// `WCO_RLS_MERGE_UPDATE_CHECK` and `WCO_RLS_MERGE_DELETE_CHECK`: the conditions of `USING` on the row of the target table of a `MERGE`.
    Target,
}

/// `OnConflictExpr`: what an `INSERT` does for a new row with the key of a row that the table has.
#[derive(Clone, Debug, PartialEq)]
pub struct OnConflict {
    /// The places in [`Write::unique`] of the arbiter indexes, which `infer_arbiter_indexes` finds. A conflict in another unique index is an error.
    pub arbiters: Vec<usize>,
    /// True for a clause with no inference. Each unique index is then an arbiter, also an index of a partition that the partitioned table does not have.
    pub all: bool,
    /// The OID of the index of the exclusion constraint of `ON CONSTRAINT`, or 0. Each exclusion constraint is an arbiter of a clause with no inference.
    pub exclusion: u32,
    /// The query of `DO UPDATE`, or `None` for `DO NOTHING`. It reads two relations: the row that the table has, with the ID of its version at the end, and `EXCLUDED`, the row that the statement proposed. It gives the new row, then the conditions of the check constraints, then the values that `RETURNING` carries, or no row when the condition of `WHERE` is not true.
    pub update: Option<Box<Query>>,
    /// The strength of the lock of `DO SELECT`, or `None` for `DO UPDATE` and `DO NOTHING`. [`OnConflict::update`] is then the query of `DO SELECT`, which gives the row that the table has, and the statement writes no row.
    pub select: Option<LockClauseStrength>,
    /// The number of values that `RETURNING` carries: the columns of the old row. A row that the statement inserts has nulls for them.
    pub carried: usize,
    /// The foreign keys that an update of a row checks, in the order of the names of their triggers.
    pub foreign: Vec<Foreign>,
    /// The checks of `WCO_RLS_CONFLICT_CHECK` of `DO UPDATE` and `DO SELECT` on the row that the table has, after the condition of `WHERE`.
    pub conflict_check: Option<PolicyCheck>,
    /// The checks of `WCO_RLS_UPDATE_CHECK` of `DO UPDATE` on the new row.
    pub update_check: Option<PolicyCheck>,
}

impl OnConflict {
    /// True for `DO SELECT` with a lock, such as `FOR UPDATE`.
    #[must_use]
    pub fn locks(&self) -> bool {
        self.select.is_some_and(|strength| strength != LockClauseStrength::LCS_NONE)
    }

    /// `show_modifytable_info`: what the statement does for a conflict, as `EXPLAIN` shows it. `DO SELECT` shows its lock.
    #[must_use]
    pub fn resolution(&self) -> &'static str {
        match self.select {
            Some(LockClauseStrength::LCS_NONE) => "SELECT",
            Some(LockClauseStrength::LCS_FORKEYSHARE) => "SELECT FOR KEY SHARE",
            Some(LockClauseStrength::LCS_FORSHARE) => "SELECT FOR SHARE",
            Some(LockClauseStrength::LCS_FORNOKEYUPDATE) => "SELECT FOR NO KEY UPDATE",
            Some(_) => "SELECT FOR UPDATE",
            None if self.update.is_some() => "UPDATE",
            None => "NOTHING",
        }
    }
}

/// What a trigger of a foreign key on the referenced table does for an old key, `confdeltype` and `confupdtype`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// `NO ACTION`: an error if a row of the referencing table has the old key and no other row of the referenced table has it.
    NoAction,
    /// `RESTRICT`: an error if a row of the referencing table has the old key. The check does not look for another row with the old key, as `ri_Check_Pk_Match` does for `NO ACTION`.
    Restrict,
    /// `CASCADE`: the rows of the referencing table with the old key go, or get the new key.
    Cascade,
    /// `SET NULL`: the rows of the referencing table with the old key get nulls.
    SetNull,
    /// `SET DEFAULT`: the rows of the referencing table with the old key get the defaults of the columns.
    SetDefault,
}

/// A foreign key that a write checks after the statement for each row that it wrote, as a trigger of `ri_triggers.c` checks it. The target table is the referencing table for `RI_FKey_check_ins` and `RI_FKey_check_upd`, and the referenced table for the triggers of the actions, such as `RI_FKey_noaction_del` and `RI_FKey_cascade_upd`.
#[derive(Clone, Debug, PartialEq)]
pub struct Foreign {
    /// The name of the constraint.
    pub name: String,
    /// True when the target table is the referencing table.
    pub referencing: bool,
    /// The action for an old key of the referenced table. It is [`Action::NoAction`] for the referencing table.
    pub action: Action,
    /// The statement of `CASCADE`, `SET NULL` or `SET DEFAULT` on the referencing table, as `ri_PlanCheck` makes it, or an empty text. Its parameters are the old key, or for `ON UPDATE CASCADE` the new key and then the old key, with the types of [`Foreign::types`].
    pub query: String,
    /// The query of `ri_restrict` on the referencing table, `SELECT 1 ... FOR KEY SHARE OF x`, for the context of an error of its privileges, or an empty text for the referencing table and for a temporal foreign key.
    pub restrict: String,
    /// `MATCH FULL`: a key with nulls and values is an error.
    pub full: bool,
    /// The OID of the other table.
    pub other: u32,
    /// The name of the other table, for the error.
    pub other_name: String,
    /// The key columns of the target table, from 0.
    pub columns: Vec<usize>,
    /// The names of the key columns of the target table, for the detail of the error.
    pub names: Vec<String>,
    /// The key columns of the other table, from 0.
    pub other_columns: Vec<usize>,
    /// The type of each column of the key of the referenced table. The checks compare the values of the referencing table as values of these types.
    pub types: Vec<u32>,
    /// The type of each column of the key of the referencing table.
    pub referencing_types: Vec<u32>,
    /// For each key column, the query `SELECT $1::type` of the cast of a referencing value to the type of the equality operator when a function of the user does the cast, or else nothing. `ri_GenerateQual` puts the cast in the query of the check, so the function runs for each key that the check looks up.
    pub casts: Vec<String>,
    /// The functions that the check of a new key calls on the referencing values, as the owner of the referenced table: the function of each equality operator and the cast functions of the user. `ExecInitFunc` and the fast path of the check ask for `EXECUTE` on them.
    pub functions: Vec<u32>,
    /// The query of the check of a new key, `SELECT 1 FROM table x WHERE key OPERATOR(op) $1 FOR KEY SHARE OF x`, when the referenced table is partitioned, as `RI_FKey_check` runs it through SPI. An error of the query has its text as the context. The check of another table takes the fast path, with no context.
    pub check: String,
    /// The OID of the constraint, which `SET CONSTRAINTS` names.
    pub constraint: u32,
    /// `tgdeferrable`: `SET CONSTRAINTS` can defer the trigger. The trigger of an action is not deferrable.
    pub deferrable: bool,
    /// `tginitdeferred`: the trigger fires at the commit when `SET CONSTRAINTS` does not change its mode.
    pub deferred: bool,
    /// `tgisclone`: the trigger is a clone of a trigger of the parent table. The delete of a row that an `UPDATE` moves to another partition skips such a trigger of the referenced table, because the event of the update of the partitioned table does its work.
    pub cloned: bool,
    /// `riinfo->hasperiod`: the last key columns are the periods of a temporal foreign key. A key of the referencing table needs keys of the referenced table with the other columns equal whose periods together cover its period.
    pub period: bool,
}

/// A unique index of the target table, which `_bt_check_unique` checks for each new version of a row.
#[derive(Clone, Debug, PartialEq)]
pub struct Unique {
    /// The OID of the index.
    pub oid: u32,
    /// The name of the index, for the error.
    pub name: String,
    /// The column of the table of each key column, from 0.
    pub columns: Vec<usize>,
    /// The type of each key column.
    pub types: Vec<u32>,
    /// The key columns as `pg_get_indexdef_columns` gives them, for the detail of the error.
    pub shown: String,
    /// `indnullsnotdistinct`: a null is equal to a null, so two keys with nulls can conflict.
    pub nulls_not_distinct: bool,
    /// The expressions and the predicate of an index with an expression or a predicate, or `None` for an index whose key is columns of the table.
    pub derived: Option<Box<Derived>>,
    /// The constraint of an index that is not `indimmediate`, or `None` for an index that checks each key at once.
    pub deferrable: Option<Deferrable>,
}

/// A `DEFERRABLE` unique constraint. A write does not fail on a duplicate key of its index: `unique_key_recheck` checks the key again after the statement, or at the commit when the constraint is deferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deferrable {
    /// The OID of the constraint, for the modes of `SET CONSTRAINTS`.
    pub constraint: u32,
    /// `condeferred`: the constraint is `INITIALLY DEFERRED`.
    pub deferred: bool,
}

/// The key of a unique index with an expression or a predicate, which `FormIndexDatum` computes for each row. A row that does not pass the predicate is not in the index, as `ExecInsertIndexTuples` checks it with `ExecQual`. [`Unique::columns`] has the places of the key columns in the values of the expressions.
#[derive(Clone, Debug, PartialEq)]
pub struct Derived {
    /// The OID of the table.
    pub table: u32,
    /// The type of each column of the table.
    pub types: Vec<u32>,
    /// The expression of each key column, with the columns of the table as columns of the relation 0. A key column that is a column of the table is a variable.
    pub exprs: Vec<Expr>,
    /// The parts of the predicate, `indpred`, or none for an index with no predicate.
    pub predicate: Vec<Expr>,
}

/// The key of the index `info` of the table `table` for `FormIndexDatum`, with the type and the type modifier of each column of the table in `columns`.
///
/// # Errors
///
/// The errors of the reading of the expressions.
pub fn index_derived(
    info: &rupg_catalog::IndexInfo,
    table: u32,
    columns: &[(u32, i32)],
) -> Result<Derived> {
    let count = usize::try_from(info.key_count).unwrap_or_default();
    let mut exprs = match &info.exprs {
        Some(text) => crate::node::read_list(text)?,
        None => Vec::new(),
    }
    .into_iter();
    let mut derived = Derived {
        table,
        types: columns.iter().map(|c| c.0).collect(),
        exprs: Vec::with_capacity(count),
        predicate: match &info.predicate {
            Some(text) => match crate::node::read(text)? {
                Expr { kind: ExprKind::Bool(BoolOp::And, parts), .. } => parts,
                part => vec![part],
            },
            None => Vec::new(),
        },
    };
    for &attnum in info.keys.iter().take(count) {
        let expr = if attnum == 0 {
            exprs.next().ok_or_else(|| Error::internal("too few entries in indexprs"))?
        } else {
            let column = usize::try_from(attnum - 1).ok().and_then(|c| columns.get(c));
            let Some(&(ty, typmod)) = column else {
                return Err(Error::internal("an index with a key that is not a column"));
            };
            let var = Var { relation: 0, attnum, levels_up: 0 };
            Expr { typmod, ..Expr::new(ExprKind::Var(var), ty) }
        };
        derived.exprs.push(expr);
    }
    Ok(derived)
}

/// The keys of the indexes with an expression or a predicate that the catalog `new` has ready on the table `table` and the catalog `old` does not, which `index_build` computes for each row of the table.
///
/// # Errors
///
/// The errors of the reading of the expressions.
pub fn built_index_keys(
    old: &rupg_catalog::Catalog,
    new: &rupg_catalog::Catalog,
    table: u32,
) -> Result<Vec<Derived>> {
    let Some(rel) = new.relation(table) else { return Ok(Vec::new()) };
    let types: Vec<(u32, i32)> = rel.columns.iter().map(|c| (c.ty, c.typmod)).collect();
    let mut indexes: Vec<_> = new
        .relations()
        .filter(|index| index.kind == RelKind::Index)
        .filter_map(|index| Some((index.oid, index.index.as_ref()?)))
        .filter(|(oid, info)| {
            info.table == table
                && info.ready
                && !info.unique
                && (info.exprs.is_some() || info.predicate.is_some())
                && old.relation(*oid).and_then(|o| o.index.as_ref()).is_none_or(|o| !o.ready)
        })
        .collect();
    indexes.sort_by_key(|(oid, _)| *oid);
    indexes.into_iter().map(|(_, info)| index_derived(info, table, &types)).collect()
}

/// `quoteOneName` of `ri_triggers.c`: a name in double quotes, with each double quote doubled.
fn quote_one(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The statement of an action of a foreign key on the referencing table `table`, as `RI_FKey_cascade_del`, `RI_FKey_cascade_upd` and `ri_set` make it. `names` are the key columns of the referencing table.
fn action_query(
    (action, kind): (Action, WriteKind),
    (table, only): (&str, bool),
    (names, set): (&[&str], &[&str]),
    foreign: &Foreign,
) -> String {
    let columns: Vec<String> = names.iter().map(|name| quote_one(name)).collect();
    // `ri_triggers.c` writes `ONLY` unless the referencing table is partitioned.
    let only = if only { "ONLY " } else { "" };
    let mut query = match (action, kind) {
        (Action::Cascade, WriteKind::Delete) => format!("DELETE FROM {only}{table}"),
        (Action::Cascade, _) => {
            let set: Vec<String> =
                columns.iter().enumerate().map(|(at, c)| format!(" {c} = ${}", at + 1)).collect();
            format!("UPDATE {only}{table} SET{}", set.join(","))
        }
        _ => {
            let value = if action == Action::SetNull { "NULL" } else { "DEFAULT" };
            let set: Vec<String> =
                set.iter().map(|c| format!(" {} = {value}", quote_one(c))).collect();
            format!("UPDATE {only}{table} SET{}", set.join(","))
        }
    };
    // The parameters of the old key come after the new key for `ON UPDATE CASCADE`.
    let first =
        if action == Action::Cascade && kind == WriteKind::Update { names.len() } else { 0 };
    query.push_str(&key_quals(names, first, foreign));
    query
}

/// `ri_GenerateQual` for each key column of the referencing table `names`: ` WHERE $n OPERATOR(pg_catalog.=) "a" AND ...`, with the parameters of the old key from `first + 1`.
fn key_quals(names: &[&str], first: usize, foreign: &Foreign) -> String {
    let mut query = String::new();
    for (at, name) in names.iter().enumerate() {
        let column = quote_one(name);
        let separator = if at == 0 { "WHERE" } else { "AND" };
        let (pk, fk) = (foreign.types[at], foreign.referencing_types[at]);
        let (left, right) = operator_types(pk, fk);
        query.push_str(&format!(" {separator} ${}", first + at + 1));
        if left != pk {
            query.push_str(&cast_to(left));
        }
        query.push_str(&format!(" OPERATOR(pg_catalog.=) {column}"));
        if right != fk {
            query.push_str(&cast_to(right));
        }
    }
    query
}

/// The input types of the equality operator `pf_eq_oprs` of a key column of the referenced type `pk` and the referencing type `fk`. `ATAddForeignKeyConstraint` takes the operator of the operator class of the index of the referenced key for the two types, or for the type of the class when the family has no operator for the type of the referencing column.
fn operator_types(pk: u32, fk: u32) -> (u32, u32) {
    let class = |ty: u32| match types::base(ty) {
        oid::VARCHAR => oid::TEXT,
        ty => ty,
    };
    let family = |ty: u32| match ty {
        oid::INT2 | oid::INT4 | oid::INT8 => 1,
        oid::FLOAT4 | oid::FLOAT8 => 2,
        oid::DATE | oid::TIMESTAMP | oid::TIMESTAMPTZ => 3,
        oid::TEXT | oid::NAME => 4,
        _ => 0,
    };
    let (left, other) = (class(pk), class(fk));
    let right = if other == left || (family(left) != 0 && family(left) == family(other)) {
        other
    } else {
        left
    };
    (left, right)
}

/// `add_cast_to` of `ruleutils.c`: the cast to a type of `pg_catalog`, with the names as `quote_identifier` gives them.
fn cast_to(ty: u32) -> String {
    let name = builtin::type_by_oid(ty).map_or("", |row| row.name);
    format!("::pg_catalog.{}", crate::ddl::quote_identifier(name))
}

/// An item of a row of `VALUES` or of `UPDATE ... SET`: an expression, or `DEFAULT` at its place.
pub(crate) enum Item {
    Expr(Expr),
    Default(Option<usize>),
}

impl Item {
    fn place(&self) -> Option<usize> {
        match self {
            Item::Expr(expr) => expr.place(),
            Item::Default(at) => *at,
        }
    }
}

/// A generated column of a table: the index of the column from 0, its generation expression and `attgenerated`, `s` for a stored column or `v` for a virtual column.
pub(crate) type Generated = (usize, Expr, u8);

/// A column that the statement gives: the index of the column in the table, the place of its name in the column list, and the subscripts after the name, which are empty when the item sets the whole column.
pub(crate) type Given = (usize, Option<usize>, List);

/// The bits in `tgtype` of a trigger `INSTEAD OF ... FOR EACH ROW`.
pub(crate) const INSTEAD_ROW: i16 = rupg_catalog::TRIGGER_ROW | rupg_catalog::TRIGGER_INSTEAD;

/// The error of a clause that the analyzer does not take yet.
pub(crate) fn not_yet(what: &str) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet"))
}

/// `exprLocation` of a list of items: the first place.
fn first_place(items: &[Item]) -> Option<usize> {
    items.iter().find_map(Item::place)
}

impl Analyzer<'_> {
    /// `transformInsertStmt`: the query tree of an `INSERT`.
    pub(crate) fn insert(&mut self, s: &InsertStmt) -> Result<Query> {
        let inverted = std::mem::take(&mut self.rule.inverted);
        self.write_with(s.withClause.as_deref())?;
        let rv = s.relation.as_deref().ok_or_else(|| Error::internal("an INSERT with no table"))?;
        let (table, name, columns) = self.target_table(rv, WriteKind::Insert)?;
        let rows = std::mem::take(&mut self.rule.rows);
        if self.auto_view.is_some() && !inverted.is_empty() {
            return Err(not_yet("a conditional rule DO INSTEAD of an updatable view"));
        }
        let given = match self.auto_view_targets(&s.cols, columns.len())? {
            Some(given) => given,
            None => self.insert_targets(&s.cols, &name, &columns)?,
        };
        let explicit = !s.cols.is_empty();
        // An `INSERT` into a view does not keep its form.
        let keep = self.keep_source && self.auto_view.is_none();
        let mut kept = None;
        let select = match &s.selectStmt {
            None => None,
            Some(Node::SelectStmt(select)) => Some(&**select),
            Some(_) => return Err(Error::internal("an INSERT source that is not SELECT")),
        };
        let mut query = match select {
            None => {
                kept = keep.then_some(InsertSource::Default);
                let row = self.full_row(table, &columns, &given, Vec::new())?;
                row_query(&columns, row)
            }
            Some(select) if general_select(select) => {
                // `rewriteValuesRTE`: `DEFAULT` in the `VALUES` that the rewrite of a rule made a subquery is the default of the column.
                if crate::rules::has_values_default(select) {
                    let mut defaults = Vec::with_capacity(given.len());
                    for (index, _, _) in &given {
                        defaults.push(self.column_default(table, *index, &columns[*index])?);
                    }
                    self.rule.defaults = Some(defaults);
                }
                let query =
                    self.insert_select(select, table, &columns, &given, explicit, s.r#override);
                self.rule.defaults = None;
                let query = query?;
                if keep && let Some(relation) = query.relations.first() {
                    let mut assigned = Vec::with_capacity(given.len());
                    for (i, (index, at, indirection)) in given.iter().enumerate() {
                        let part = match relation.columns.get(i) {
                            Some(c) if !indirection.is_empty() => {
                                let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
                                let var = Var { relation: 0, attnum, levels_up: 0 };
                                let mut value = Expr::new(ExprKind::Var(var), c.ty);
                                value.typmod = c.typmod;
                                let column = &columns[*index];
                                Some(self.assign_part(column, indirection, *at, value, None)?)
                            }
                            _ => None,
                        };
                        assigned.push(part);
                    }
                    if let Some(sub) = relation.subquery.clone() {
                        kept = Some(InsertSource::Select(sub, assigned));
                    }
                }
                query
            }
            Some(select) if select.valuesLists.len() > 1 => {
                let mut rows = keep.then(Vec::new);
                let query = self.insert_values(
                    select,
                    table,
                    &columns,
                    (&given, explicit, s.r#override),
                    rows.as_mut(),
                )?;
                kept = rows.map(InsertSource::Values);
                query
            }
            Some(select) => {
                let Some(Some(Node::List(list))) = select.valuesLists.first() else {
                    return Err(Error::internal("a VALUES row that is not a list"));
                };
                self.srf_values = true;
                let items = self.value_items(list, Kind::ValuesSingle);
                self.srf_values = false;
                let items = items?;
                if keep {
                    kept = Some(InsertSource::Row(self.kept_row(&items, &columns, &given)?));
                }
                let exprs =
                    self.insert_row(items, table, &columns, &given, explicit, s.r#override)?;
                let row = self.full_row(table, &columns, &given, exprs)?;
                row_query(&columns, row)
            }
        };
        if rows {
            // The rows of `*rule*`: the new row, with its generated columns.
            let generated = self.generated_columns(table)?;
            let mut query = with_checks(&name, &columns, query, Vec::new(), &generated, &[]);
            self.end_write_with(&mut query, s.withClause.as_deref())?;
            self.rule.columns = columns;
            return Ok(query);
        }
        if !inverted.is_empty() {
            let generated = self.generated_columns(table)?;
            let mut rows = with_checks(&name, &columns, query, Vec::new(), &[], &[]);
            let new = crate::rules::relation_row(&columns, 0, 0);
            let new = crate::rules::with_generated(new, &generated);
            let new = crate::rules::named_row(&columns, new);
            let quals = self.inverted_quals(&inverted, None, Some(new))?;
            rows.filter = crate::rules::and_filter(rows.filter.take(), quals);
            query = rows;
        }
        // `ExecFindPartition` routes each row, and the partition then checks it.
        let checks = if self.is_partitioned(table) { Vec::new() } else { self.checks(table)? };
        let names = checks.iter().map(|(n, _)| n.clone()).collect();
        let conflict_set = s
            .onConflictClause
            .as_deref()
            .filter(|c| matches!(c.action, OnConflictAction::ONCONFLICT_UPDATE))
            .map(|c| &c.targetList);
        // The items of `SET` of an updatable view name the columns of its base relation.
        let renamed = conflict_set.and_then(|list| self.auto_view_set(rv, list, &columns));
        let conflict_set = renamed.as_ref().map(|(list, _, _)| list).or(conflict_set);
        let generated = self.generated_columns(table)?;
        let mut recheck =
            self.recheck(table, WriteKind::Insert, &name, &columns, &checks, &generated);
        if recheck.is_none() && conflict_set.is_some() {
            // The triggers `BEFORE UPDATE ... FOR EACH ROW` of `DO UPDATE` also come before the checks.
            recheck = self.recheck(table, WriteKind::Update, &name, &columns, &checks, &generated);
        }
        let updated = conflict_set.map_or_else(Vec::new, |list| set_columns(list, &columns));
        // `DO SELECT` with a lock needs `UPDATE` too.
        let locks = s.onConflictClause.as_deref().is_some_and(|c| {
            c.action == OnConflictAction::ONCONFLICT_SELECT
                && c.lockStrength != LockClauseStrength::LCS_NONE
        });
        if conflict_set.is_some() || locks {
            self.mark_updated(&updated);
        }
        let updated = crate::modify::with_extra_updated(updated, &generated);
        if !checks.is_empty() || !generated.is_empty() {
            query = with_checks(&name, &columns, query, checks, &generated, &[]);
        }
        let width = columns.len();
        let unique = self.unique_keys(table, &columns)?;
        let foreign = self.foreign_keys(table, WriteKind::Insert, &columns)?;
        let (mut on_conflict, mut returning, mut conflict, mut returning_tid) =
            (None, None, None, None);
        if s.onConflictClause.is_some() || s.returningClause.is_some() {
            let outer = std::mem::take(&mut self.scope);
            // `ON CONFLICT` and `RETURNING` can read the queries of `WITH`.
            self.scope.ctes.clone_from(&outer.ctes);
            let result = self.insert_clauses(s, rv, table, &name, &columns, &unique);
            self.scope = outer;
            (on_conflict, returning, conflict, returning_tid) = result?;
        }
        let alias = rv.alias.as_ref().and_then(|a| a.aliasname.as_ref()).map(ToString::to_string);
        let insert_def = kept.map(|source| {
            let width = match &source {
                InsertSource::Default => 0,
                InsertSource::Row(row) => row.len(),
                InsertSource::Values(rows) => rows.first().map_or(0, Vec::len),
                InsertSource::Select(sub, _) => sub.targets.iter().filter(|t| !t.junk).count(),
            };
            let mut target = Relation::new(table, columns.clone(), None, None, None);
            target.name = alias.clone().unwrap_or_else(|| name.clone());
            target.alias = alias.as_ref().map(|_| Vec::new());
            let columns = given
                .iter()
                .take(width)
                .map(|(index, _, _)| i16::try_from(index + 1).unwrap_or(i16::MAX))
                .collect();
            let conflict = conflict.take();
            Box::new(InsertDef { target, columns, overriding: s.r#override, source, conflict })
        });
        query.write = Some(Write {
            kind: WriteKind::Insert,
            table,
            name,
            alias,
            width,
            checks: names,
            unique,
            returning,
            returning_tid,
            foreign,
            on_conflict,
            merge: None,
            recheck,
            updated,
            instead: self.is_view(table),
            inherit: false,
            view_check: None,
            policy_check: None,
            views: 0,
            modified: Vec::new(),
            insert_def,
            set_def: None,
            pruned: true,
        });
        self.end_write_with(&mut query, s.withClause.as_deref())?;
        Ok(query)
    }

    /// `transformOnConflictClause` and `transformReturningClause` of an `INSERT`, which see only the target table. The query of `DO UPDATE` gets the conditions of the check constraints and the values that `RETURNING` carries.
    fn insert_clauses(
        &mut self,
        s: &InsertStmt,
        rv: &RangeVar,
        table: u32,
        name: &str,
        columns: &[Column],
        unique: &[Unique],
    ) -> Result<InsertClauses> {
        if let Some(clause) = s.onConflictClause.as_deref()
            && clause.action == OnConflictAction::ONCONFLICT_SELECT
            && s.returningClause.is_none()
        {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "ON CONFLICT DO SELECT requires a RETURNING clause",
            )
            .at_opt(place(clause.location)));
        }
        self.target_from(rv, &[])?;
        let (mut on_conflict, inference, def) = match s.onConflictClause.as_deref() {
            None => (None, Inference::All, None),
            Some(clause) => {
                let (on_conflict, inference, def) =
                    self.on_conflict(clause, rv, table, name, columns)?;
                (Some(on_conflict), inference, Some(def))
            }
        };
        // With `DO UPDATE`, the old row of `RETURNING` is the row that the statement updates, as for an `UPDATE`. With `DO SELECT`, the old and the new rows are the row that the table has.
        let update = on_conflict.as_ref().is_some_and(|c| c.update.is_some());
        let kind = if update { WriteKind::Update } else { WriteKind::Insert };
        let (returning, carried, returning_tid) = match s.returningClause.as_deref() {
            None => (None, Vec::new(), None),
            Some(clause) => {
                let (query, carried, tid) = self.returning(clause, kind, true)?;
                (Some(Box::new(query)), carried, tid)
            }
        };
        if let Some(conflict) = &mut on_conflict
            && let Some(mut query) = conflict.update.take()
        {
            let tail: Vec<Column> = carried
                .iter()
                .map(|expr| Column {
                    name: String::new(),
                    ty: expr.ty,
                    typmod: expr.typmod,
                    not_null: false,
                })
                .collect();
            conflict.carried = carried.len();
            query.targets.extend(carried_targets(carried));
            // The leaf partition checks the new row of a partitioned table, and `DO SELECT` writes no row.
            let write = conflict.select.is_none() && !self.is_partitioned(table);
            let checks = if write { self.checks(table)? } else { Vec::new() };
            let generated =
                if conflict.select.is_none() { self.generated_columns(table)? } else { Vec::new() };
            if !checks.is_empty() || !generated.is_empty() {
                query = Box::new(with_checks(name, columns, *query, checks, &generated, &tail));
            }
            conflict.update = Some(query);
        }
        // The planner matches the inference with the indexes after the analysis.
        if let Some(conflict) = &mut on_conflict {
            conflict.all = matches!(inference, Inference::All);
            if let Inference::Constraint(index) = inference
                && !unique.iter().any(|key| key.oid == index)
            {
                conflict.exclusion = index;
            }
            conflict.arbiters =
                arbiters(inference, unique, self.env.catalog(), self.is_partitioned(table))?;
        }
        Ok((on_conflict, returning, def, returning_tid))
    }

    /// `transformOnConflictClause`: the inference of the arbiter indexes, and the query of `DO UPDATE` with the new row. `SET` and `WHERE` see the target table and `EXCLUDED`, and the clauses after them see only the target table.
    fn on_conflict(
        &mut self,
        clause: &OnConflictClause,
        rv: &RangeVar,
        table: u32,
        name: &str,
        columns: &[Column],
    ) -> Result<(OnConflict, Inference, ConflictDef)> {
        let (update, select) = match clause.action {
            OnConflictAction::ONCONFLICT_NOTHING => (false, None),
            OnConflictAction::ONCONFLICT_UPDATE => (true, None),
            _ => (false, Some(clause.lockStrength)),
        };
        let (inference, mut def) =
            self.inference(clause, update || select.is_some(), table, name, columns)?;
        def.select = select;
        let mut on_conflict = OnConflict {
            arbiters: Vec::new(),
            all: false,
            exclusion: 0,
            update: None,
            select,
            carried: 0,
            foreign: Vec::new(),
            conflict_check: None,
            update_check: None,
        };
        if select.is_some() {
            // `DO SELECT` gives the row that the table has when `WHERE` lets it through.
            let excluded = self.add_excluded()?;
            let filter = self.target_where(clause.whereClause.as_ref());
            self.hide_last_item();
            let row = columns.iter().enumerate().map(|(at, c)| column_var(at, c)).collect();
            let query = Query {
                relations: self.scope.relations.clone(),
                from: vec![FromItem::Relation(0), FromItem::Relation(excluded)],
                targets: targets(columns, row),
                filter: filter?,
                ..Query::default()
            };
            def.relations = excluded_named(&query.relations, excluded);
            def.filter.clone_from(&query.filter);
            on_conflict.update = Some(Box::new(query));
            return Ok((on_conflict, inference, def));
        }
        if !update {
            return Ok((on_conflict, inference, def));
        }
        let excluded = self.add_excluded()?;
        // The items of `SET` of an updatable view name the columns of its base relation, and they must name updatable columns of the view.
        let (renamed, error) = match self.auto_view_set(rv, &clause.targetList, columns) {
            Some((list, unknown, fixed)) => (Some(list), unknown.or(fixed)),
            None => (None, None),
        };
        let list = renamed.as_ref().unwrap_or(&clause.targetList);
        let mut set = Vec::new();
        let row = match error {
            Some(error) => self.set_items(list).and(Err(error)),
            None => self.update_row(rv, table, name, columns, list, Some(&mut set)),
        };
        let parts = row.and_then(|row| {
            let filter = self.target_where(clause.whereClause.as_ref())?;
            Ok((row, filter))
        });
        self.hide_last_item();
        let (row, filter) = parts?;
        let query = Query {
            relations: self.scope.relations.clone(),
            from: vec![FromItem::Relation(0), FromItem::Relation(excluded)],
            targets: targets(columns, row),
            filter,
            ..Query::default()
        };
        def.relations = excluded_named(&query.relations, excluded);
        def.filter.clone_from(&query.filter);
        def.set = Some(set);
        on_conflict.update = Some(Box::new(query));
        on_conflict.foreign = self.foreign_keys(table, WriteKind::Update, columns)?;
        Ok((on_conflict, inference, def))
    }

    /// `transformOnConflictArbiter`: the columns of the inference, or the index of the constraint that the clause names, and the clause as [`ConflictDef`] shows it.
    fn inference(
        &mut self,
        clause: &OnConflictClause,
        update: bool,
        table: u32,
        name: &str,
        columns: &[Column],
    ) -> Result<(Inference, ConflictDef)> {
        let mut def = ConflictDef::default();
        let Some(infer) = clause.infer.as_deref() else {
            if update {
                let what = if clause.action == OnConflictAction::ONCONFLICT_SELECT {
                    "SELECT"
                } else {
                    "UPDATE"
                };
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!(
                        "ON CONFLICT DO {what} requires inference specification or constraint name"
                    ),
                )
                .with_hint("For example, ON CONFLICT (column_name).")
                .at_opt(place(clause.location)));
            }
            return Ok((Inference::All, def));
        };
        if let Some(conname) = infer.conname.as_deref() {
            // The constraint is a constraint of the view that the statement names, which has none.
            let (table, name) = self.auto_view_target().unwrap_or((table, name));
            let constraint = self
                .env
                .catalog()
                .and_then(|catalog| catalog.constraints_of(table).find(|c| c.name == conname));
            let Some(constraint) = constraint else {
                return Err(Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("constraint \"{conname}\" for table \"{name}\" does not exist"),
                ));
            };
            // `infer_arbiter_indexes`: an exclusion constraint is an arbiter only of `DO NOTHING`.
            if constraint.is_exclusion() {
                let action = match clause.action {
                    OnConflictAction::ONCONFLICT_NOTHING => None,
                    OnConflictAction::ONCONFLICT_UPDATE => Some("UPDATE"),
                    _ => Some("SELECT"),
                };
                if let Some(action) = action {
                    return Err(Error::new(
                        SqlState::WRONG_OBJECT_TYPE,
                        format!("ON CONFLICT DO {action} not supported with exclusion constraints"),
                    ));
                }
            }
            // The columns of the constraint need `SELECT`.
            let keys = constraint.keys.clone();
            let index = constraint.index;
            def.constraint = constraint.oid;
            if let Some(perm) = self.target_perm_mut() {
                perm.required |= ACL_SELECT;
                perm.selected.extend(keys);
            }
            return Ok((Inference::Constraint(index), def));
        }
        let mut set = Vec::with_capacity(infer.indexElems.len());
        let mut exprs = Vec::new();
        let mut exact = Vec::new();
        for node in &infer.indexElems {
            let Some(Node::IndexElem(elem)) = node else {
                return Err(Error::internal("an inference element that is not IndexElem"));
            };
            let at = place(elem.location);
            let not_allowed = |what: &str| {
                Error::new(
                    SqlState::INVALID_COLUMN_REFERENCE,
                    format!("{what} is not allowed in ON CONFLICT clause"),
                )
                .at_opt(at)
            };
            if elem.ordering != SortByDir::SORTBY_DEFAULT {
                return Err(not_allowed("ASC/DESC"));
            }
            if elem.nulls_ordering != SortByNulls::SORTBY_NULLS_DEFAULT {
                return Err(not_allowed("NULLS FIRST/LAST"));
            }
            if !elem.opclassopts.is_empty() {
                return Err(Error::new(
                    SqlState::INVALID_COLUMN_REFERENCE,
                    "operator class options are not allowed in ON CONFLICT clause",
                )
                .at_opt(at));
            }
            let collation = match elem.collation.is_empty() {
                true => 0,
                false => self.collation_oid(&elem.collation, at)?,
            };
            let (oid, class) = match elem.opclass.is_empty() {
                true => ((0, 0), None),
                false => {
                    let (oid, family, intype) = self.btree_opclass(&elem.opclass)?;
                    ((oid, intype), Some((family, intype)))
                }
            };
            let mut note = |target: Exact| {
                if collation != 0 || class.is_some() {
                    exact.push(ExactElem { target, collation, class });
                }
            };
            let Some(column) = elem.name.as_deref() else {
                // `resolve_unique_index_expr`: an expression that is a column counts as the column.
                let expr =
                    self.with_kind(Kind::IndexExpression, |a| a.transform(elem.expr.as_ref()))?;
                def.elems.push((expr.clone(), collation, oid.0, oid.1));
                match expr.kind {
                    ExprKind::Var(var) if var.levels_up == 0 && var.attnum > 0 => {
                        note(Exact::Column(var.attnum));
                        set.extend(usize::try_from(var.attnum - 1).ok());
                    }
                    _ => {
                        note(Exact::Expr(exprs.len()));
                        exprs.push(expr);
                    }
                }
                continue;
            };
            // A column of an updatable view is the expression of the view on the base relation.
            let index = match self.auto_view_column(column) {
                None => columns.iter().position(|c| c.name == column),
                Some(Some(expr)) => match expr.kind {
                    ExprKind::Var(var) if var.attnum > 0 => usize::try_from(var.attnum - 1).ok(),
                    _ => {
                        note(Exact::Expr(exprs.len()));
                        exprs.push(expr);
                        continue;
                    }
                },
                Some(None) => None,
            };
            let Some(index) = index else {
                return Err(Error::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!("column \"{column}\" does not exist"),
                )
                // `resolve_unique_index_expr` puts the column reference at the inference clause.
                .at_opt(place(infer.location)));
            };
            // The column is a reference to the column, which needs `SELECT`.
            if let Some(perm) = self.target_perm_mut() {
                perm.required |= ACL_SELECT;
                perm.selected.insert(i16::try_from(index + 1).unwrap_or(i16::MAX));
            }
            note(Exact::Column(i16::try_from(index + 1).unwrap_or(i16::MAX)));
            def.elems.push((column_var(index, &columns[index]), collation, oid.0, oid.1));
            set.push(index);
        }
        set.sort_unstable();
        set.dedup();
        let mut predicate = Vec::new();
        if let Some(clause) = infer.whereClause.as_ref() {
            let expr = self.with_kind(Kind::IndexPredicate, |a| a.transform(Some(clause)))?;
            let expr = self.coerce_to_boolean(expr, "WHERE")?;
            def.predicate = Some(expr.clone());
            match expr.kind {
                ExprKind::Bool(BoolOp::And, parts) => predicate = parts,
                _ => predicate.push(expr),
            }
        }
        Ok((Inference::Columns { columns: set, exprs, predicate, exact }, def))
    }

    /// `get_opclass_oid` for `btree`: the OID, the operator family and the input type of the operator class that the name gives, in the schema of the name or in the first schema of the search path that has it.
    fn btree_opclass(&self, list: &[Option<Node>]) -> Result<(u32, u32, u32)> {
        const BTREE: u32 = 403;
        let parts = crate::typename::names(list);
        let (schema, name) = self.split_name(&parts, None)?;
        let catalog = self.env.catalog();
        let find = |namespace: u32| {
            let user = catalog.and_then(|c| {
                c.opclasses()
                    .find(|c| c.method == BTREE && c.namespace == namespace && c.name == name)
                    .map(|c| (c.oid, c.family, c.intype))
            });
            user.or_else(|| {
                let row = builtin::named(Named::Opclass)
                    .iter()
                    .find(|r| r.method == BTREE && r.namespace == namespace && r.name == name)?;
                let class = builtin::opclasses().iter().find(|c| c.oid == row.oid)?;
                Some((row.oid, class.family, class.intype))
            })
        };
        let found = match schema {
            Some(namespace) => find(namespace),
            None => self.path.iter().find_map(|&namespace| find(namespace)),
        };
        found.ok_or_else(|| {
            Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!(
                    "operator class \"{}\" does not exist for access method \"btree\"",
                    parts.join(".")
                ),
            )
        })
    }

    /// `setTargetTable`: the OID, the name and the columns of the table that the statement writes. A table with a constraint or an object that the executor does not check yet gives `0A000`. The table gets the privileges that the write needs, which a `MERGE` gives later from its clauses.
    pub(crate) fn target_table(
        &mut self,
        rv: &RangeVar,
        kind: WriteKind,
    ) -> Result<(u32, String, Vec<Column>)> {
        let what = match kind {
            WriteKind::Insert => "INSERT into",
            WriteKind::Update => "UPDATE of",
            WriteKind::Delete => "DELETE from",
            WriteKind::Merge => "MERGE into",
        };
        let at = place(rv.location);
        let name = rv.relname.as_deref().unwrap_or_default().to_string();
        self.check_target(rv)?;
        let Some(oid) = self.lookup_relation(rv)? else {
            let shown = match &rv.schemaname {
                Some(schema) => format!("{schema}.{name}"),
                None => name.clone(),
            };
            return Err(Error::new(
                SqlState::UNDEFINED_TABLE,
                format!("relation \"{shown}\" does not exist"),
            )
            .at_opt(at));
        };
        let need = match kind {
            WriteKind::Insert => ACL_INSERT,
            WriteKind::Update => ACL_UPDATE,
            WriteKind::Delete => ACL_DELETE,
            WriteKind::Merge => 0,
        };
        let rel = self.env.catalog().and_then(|c| c.relation(oid));
        let Some(rel) = rel else {
            // `ExecCheckPermissions`: only a superuser can write a system catalog.
            if let Some(class) = builtin::class_by_oid(oid)
                && matches!(class.kind, b'r' | b't')
                && need != 0
                && !self.env.relation_allowed(oid, need)
            {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for table {}", class.name),
                ));
            }
            return Err(not_yet(&format!("{what} a system catalog")));
        };
        // `ExecCheckPermissions` comes before `CheckValidResultRel`.
        if matches!(rel.kind, RelKind::Sequence | RelKind::MatView)
            && self.env.filling() != oid
            && need != 0
            && !self.env.relation_allowed(oid, need)
        {
            let shown =
                if rel.kind == RelKind::Sequence { "sequence" } else { "materialized view" };
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for {shown} {}", rel.name),
            ));
        }
        match rel.kind {
            RelKind::Table => {}
            RelKind::Sequence => {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("cannot change sequence \"{name}\""),
                ));
            }
            // `view_has_instead_trigger`: the triggers `INSTEAD OF` of the view do the write.
            RelKind::View
                if kind != WriteKind::Merge
                    && self.user_triggers(oid, INSTEAD_ROW | trigger_event(kind)) => {}
            // The rows of a statement for the rules of a view need no write of the view.
            RelKind::View if self.rule.rows => {}
            // `rewriteTargetView` writes the base relation of the view.
            RelKind::View => {}
            RelKind::Partitioned => {}
            // `CheckValidResultRel`: the foreign-data wrapper of the table does the write.
            RelKind::Foreign => {}
            RelKind::MatView if self.env.filling() == oid => {}
            RelKind::MatView => {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("cannot change materialized view \"{name}\""),
                ));
            }
            _ => {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("cannot change relation \"{name}\""),
                ));
            }
        }
        if let Some(catalog) = self.env.catalog() {
            let mut rules =
                catalog.rules().filter(|r| r.relation == oid && crate::rules::fires(r, self.env));
            if kind == WriteKind::Merge {
                // `RewriteQuery`: a rule of an event other than `SELECT` stops a `MERGE`.
                if rules.any(|r| r.event != b'1') {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!("cannot execute MERGE on relation \"{name}\""),
                    )
                    .with_detail("MERGE is not supported for relations with rules."));
                }
            } else {
                let code = match kind {
                    WriteKind::Insert => b'3',
                    WriteKind::Update => b'2',
                    _ => b'4',
                };
                // [`crate::query_rewrite`] gives the statements of the rules, and a statement that it does not rewrite cannot write the table yet.
                let done = self.rule.rows || self.rule.defining || self.rule.fired == Some(oid);
                if !done && rules.any(|r| r.event == code) {
                    return Err(not_yet(&format!("{what} a table with rules")));
                }
            }
        }
        let columns = write_columns(rel);
        let name = rel.name.clone();
        let required = match kind {
            // `refresh_matview_datafill` and `intorel_receive` write the rows with no check of `INSERT`.
            WriteKind::Insert if self.env.filling() == oid => 0,
            WriteKind::Insert => ACL_INSERT,
            WriteKind::Update => ACL_UPDATE,
            WriteKind::Delete => ACL_DELETE,
            WriteKind::Merge => 0,
        };
        self.target_perm = Some(self.add_perm(oid, required));
        // `view_has_instead_trigger`: a `MERGE` with only `DO NOTHING` needs no trigger, and it reads the view as a view with triggers `INSTEAD OF` does. `transformRuleStmt` does not check that the view of an action can be updated, as the rewrite of the action checks it.
        let auto = rel.kind == RelKind::View
            && !self.rule.rows
            && !self.rule.defining
            && !self.view_has_instead_trigger(oid, kind);
        if !auto {
            return Ok((oid, name, columns));
        }
        let view = self.auto_view(oid, &name, kind)?;
        let base = view.base_oid();
        let rel = self.env.catalog().and_then(|c| c.relation(base));
        let rel = rel.ok_or_else(|| Error::internal("an updatable view with no base relation"))?;
        let columns = write_columns(rel);
        let name = rel.name.clone();
        self.auto_view = Some(view);
        Ok((base, name, columns))
    }

    /// True when the table has a trigger of the user that fires in the mode of the session and whose type has all the bits `bits`, such as `TRIGGER_ROW | TRIGGER_BEFORE | TRIGGER_INSERT`. With no bits, any such trigger counts.
    pub(crate) fn user_triggers(&self, table: u32, bits: i16) -> bool {
        let Some(catalog) = self.env.catalog() else { return false };
        let replica = self.env.replica();
        catalog.triggers().any(|t| user_trigger(t, table, replica) && t.kind & bits == bits)
    }

    /// True when the target of the write is a view, which only a view with a trigger `INSTEAD OF` for the event can be.
    pub(crate) fn is_view(&self, table: u32) -> bool {
        crate::ddl::relkind(self.env, table) == b'v'
    }

    /// [`Write::inherit`] for the target of an `UPDATE`, a `DELETE` or a `MERGE`, the relation 0: a table with children of `INHERITS` and no `ONLY`.
    pub(crate) fn inherits_target(&self, kind: WriteKind) -> bool {
        let Some(target) = self.scope.relations.first() else { return false };
        matches!(kind, WriteKind::Update | WriteKind::Delete | WriteKind::Merge)
            && !target.only
            && crate::ddl::relkind(self.env, target.oid) == b'r'
            && self.env.catalog().is_some_and(|c| !c.children(target.oid).is_empty())
    }

    /// True for a partitioned table. Its partitions check the rows of a write, and each row of the query of an `UPDATE` or a `DELETE` gives the OID of its partition before the ID of its version.
    pub(crate) fn is_partitioned(&self, table: u32) -> bool {
        crate::ddl::relkind(self.env, table) == b'p'
    }

    /// [`Write::recheck`] for a write of the table with a trigger `BEFORE ... FOR EACH ROW` for the event of `kind`, or `None`.
    pub(crate) fn recheck(
        &self,
        table: u32,
        kind: WriteKind,
        name: &str,
        columns: &[Column],
        checks: &[(String, Expr)],
        generated: &[Generated],
    ) -> Option<Box<Query>> {
        let bits = rupg_catalog::TRIGGER_ROW | rupg_catalog::TRIGGER_BEFORE | trigger_event(kind);
        // The triggers of a partitioned table fire as their clones in the leaf partitions, which check the rows.
        if self.is_partitioned(table) || !self.user_triggers(table, bits) {
            return None;
        }
        let mut query =
            with_checks(name, columns, Query::default(), checks.to_vec(), generated, &[]);
        // The relation reads the row that the triggers give, not a subquery.
        let mut relation = Relation::new(0, columns.to_vec(), None, None, None);
        relation.name = name.to_string();
        query.relations = vec![relation];
        Some(Box::new(query))
    }

    /// The foreign keys that a write of the table checks after the statement, in the order of the names of their triggers, as `AfterTriggerSaveEvent` queues the triggers for each row.
    pub(crate) fn foreign_keys(
        &self,
        table: u32,
        kind: WriteKind,
        columns: &[Column],
    ) -> Result<Vec<Foreign>> {
        let Some(catalog) = self.env.catalog() else {
            return Ok(Vec::new());
        };
        // `AfterTriggerSaveEvent`: the leaf partitions fire the triggers of the rows. A partitioned table fires only the triggers of its referenced keys, for the update of a row that moves to another partition.
        let partitioned = catalog.relation(table).is_some_and(|r| r.kind == RelKind::Partitioned);
        if partitioned && kind != WriteKind::Update {
            return Ok(Vec::new());
        }
        let mut triggers: Vec<_> = catalog
            .triggers()
            .filter(|t| t.relation == table && t.fires(self.env.replica()))
            .filter_map(|t| {
                let function = rupg_catalog::foreign_key_function(t.function)?;
                let event = match function {
                    0 => WriteKind::Insert,
                    f if f % 2 == 1 => WriteKind::Update,
                    _ => WriteKind::Delete,
                };
                (event == kind && !(partitioned && function < 2)).then_some((t, function))
            })
            .collect();
        triggers.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        let mut list = Vec::with_capacity(triggers.len());
        for (trigger, function) in triggers {
            let constraint = catalog.constraint(trigger.constraint);
            let Some((constraint, foreign)) =
                constraint.and_then(|c| c.foreign.as_ref().map(|f| (c, f)))
            else {
                return Err(Error::internal("a trigger of a foreign key with no foreign key"));
            };
            let action = match function {
                2 | 3 => Action::Cascade,
                4 | 5 => Action::Restrict,
                6 | 7 => Action::SetNull,
                8 | 9 => Action::SetDefault,
                _ => Action::NoAction,
            };
            let referencing = function < 2;
            let (other, mine, theirs) = if referencing {
                (foreign.table, &constraint.keys, &foreign.keys)
            } else {
                (constraint.relation, &foreign.keys, &constraint.keys)
            };
            let Some(other_rel) = catalog.relation(other) else {
                return Err(Error::internal("a foreign key of a table that does not exist"));
            };
            let place = |attnum: i16| usize::try_from(attnum - 1).ok();
            let mut check = Foreign {
                name: constraint.name.clone(),
                referencing,
                action,
                query: String::new(),
                restrict: String::new(),
                full: foreign.match_type == 'f',
                other,
                other_name: other_rel.name.clone(),
                columns: Vec::with_capacity(mine.len()),
                names: Vec::with_capacity(mine.len()),
                other_columns: Vec::with_capacity(mine.len()),
                types: Vec::with_capacity(mine.len()),
                referencing_types: Vec::with_capacity(mine.len()),
                casts: Vec::with_capacity(mine.len()),
                functions: Vec::new(),
                check: String::new(),
                constraint: constraint.oid,
                deferrable: trigger.deferrable,
                deferred: trigger.deferred,
                cloned: trigger.parent != 0,
                period: foreign.period,
            };
            let mut quals = Vec::with_capacity(mine.len());
            for (i, (&attnum, &other_attnum)) in mine.iter().zip(theirs).enumerate() {
                let column = place(attnum).and_then(|at| columns.get(at).map(|c| (at, c)));
                let other_column =
                    place(other_attnum).and_then(|at| other_rel.columns.get(at).map(|c| (at, c)));
                let (Some((at, column)), Some((other_at, other_column))) = (column, other_column)
                else {
                    return Err(Error::internal("a foreign key with a key that is not a column"));
                };
                check.columns.push(at);
                check.names.push(column.name.clone());
                check.other_columns.push(other_at);
                let (key, referencing_key) = if referencing {
                    (other_column.ty, column.ty)
                } else {
                    (column.ty, other_column.ty)
                };
                check.types.push(key);
                check.referencing_types.push(referencing_key);
                let (_, right) = operator_types(key, referencing_key);
                let user_cast = right != referencing_key
                    && builtin::type_by_oid(right).is_some()
                    && catalog.casts().any(|cast| {
                        cast.source == referencing_key && cast.target == right && cast.func != 0
                    });
                check.casts.push(if user_cast {
                    format!("SELECT $1{}", cast_to(right))
                } else {
                    String::new()
                });
                if referencing {
                    let op = foreign.pf_eq.get(i).copied().unwrap_or_default();
                    let (code, op_name, op_right) = match catalog.operator(op) {
                        Some(o) => {
                            let schema = match catalog.schema(o.namespace) {
                                Some(schema) => crate::ddl::quote_identifier(&schema.name),
                                None => builtin::named_by_oid(Named::Namespace, o.namespace)
                                    .map_or_else(String::new, |n| {
                                        crate::ddl::quote_identifier(n.name)
                                    }),
                            };
                            (o.code, format!("{schema}.{}", o.name), o.right)
                        }
                        None => builtin::operator_by_oid(op).map_or_else(
                            || (0, "pg_catalog.=".to_string(), right),
                            |o| (o.code, format!("pg_catalog.{}", o.name), o.right),
                        ),
                    };
                    if code != 0 {
                        check.functions.push(code);
                    }
                    if user_cast {
                        let cast = catalog
                            .casts()
                            .find(|cast| cast.source == referencing_key && cast.target == right);
                        check.functions.extend(cast.map(|cast| cast.func));
                    }
                    // `ri_GenerateQual`: the referencing value gets a cast to the right input of the operator.
                    let cast = if op_right != referencing_key
                        && builtin::type_by_oid(op_right).is_some()
                    {
                        cast_to(op_right)
                    } else {
                        String::new()
                    };
                    quals.push(format!(
                        "{} OPERATOR({op_name}) ${}{cast}",
                        quote_one(&other_column.name),
                        i + 1
                    ));
                }
            }
            let names: Vec<&str> =
                check.other_columns.iter().map(|&at| other_rel.columns[at].name.as_str()).collect();
            let schema = match catalog.schema(other_rel.namespace) {
                Some(schema) => schema.name.clone(),
                None => builtin::named_by_oid(Named::Namespace, other_rel.namespace)
                    .map_or_else(String::new, |n| n.name.to_string()),
            };
            let table = format!("{}.{}", quote_one(&schema), quote_one(&other_rel.name));
            let only = other_rel.kind != RelKind::Partitioned;
            if referencing && !only && !foreign.period {
                check.check = format!(
                    "SELECT 1 FROM {table} x WHERE {} FOR KEY SHARE OF x",
                    quals.join(" AND ")
                );
            }
            if !referencing && !foreign.period {
                let only = if only { "ONLY " } else { "" };
                check.restrict = format!(
                    "SELECT 1 FROM {only}{table} x{} FOR KEY SHARE OF x",
                    key_quals(&names, 0, &check)
                );
            }
            if matches!(action, Action::Cascade | Action::SetNull | Action::SetDefault) {
                // `ri_set` for a delete: the columns of `confdelsetcols`, or else all the referencing columns.
                let set: Vec<&str> =
                    if kind == WriteKind::Delete && !foreign.del_set_cols.is_empty() {
                        foreign
                            .del_set_cols
                            .iter()
                            .filter_map(|&attnum| {
                                place(attnum).and_then(|at| other_rel.columns.get(at))
                            })
                            .map(|c| c.name.as_str())
                            .collect()
                    } else {
                        names.clone()
                    };
                check.query = action_query((action, kind), (&table, only), (&names, &set), &check);
            }
            list.push(check);
        }
        Ok(list)
    }

    /// The unique indexes of a table in the order of their OIDs. The index of a partitioned table has no storage: the indexes of the partitions check the rows, and it is an arbiter of `ON CONFLICT`. An index that is not ready, from a failed `CREATE INDEX CONCURRENTLY`, checks no row. The index of a constraint `WITHOUT OVERLAPS` is an exclusion constraint, which [`crate::exclusions`] gives.
    pub(crate) fn unique_keys(&self, table: u32, columns: &[Column]) -> Result<Vec<Unique>> {
        let Some(catalog) = self.env.catalog() else {
            return Ok(Vec::new());
        };
        let mut indexes: Vec<_> = catalog
            .relations()
            .filter(|rel| matches!(rel.kind, RelKind::Index | RelKind::PartitionedIndex))
            .filter_map(|rel| {
                rel.index
                    .as_ref()
                    .filter(|info| info.table == table && info.unique && info.ready)
                    .map(|info| (rel, info))
            })
            .filter(|(rel, _)| {
                !catalog.constraints_of(table).any(|c| c.index == rel.oid && c.is_exclusion())
            })
            .collect();
        indexes.sort_by_key(|(rel, _)| rel.oid);
        let mut list = Vec::with_capacity(indexes.len());
        for (rel, info) in indexes {
            let deferrable = if info.immediate {
                None
            } else {
                let constraint = catalog.constraints_of(table).find(|c| c.index == rel.oid);
                constraint.map(|c| Deferrable { constraint: c.oid, deferred: c.deferred })
            };
            let count = usize::try_from(info.key_count).unwrap_or_default();
            let mut key = Unique {
                oid: rel.oid,
                name: rel.name.clone(),
                columns: Vec::with_capacity(count),
                types: Vec::with_capacity(count),
                shown: String::new(),
                nulls_not_distinct: info.nulls_not_distinct,
                derived: None,
                deferrable,
            };
            if info.exprs.is_some() || info.predicate.is_some() {
                let types: Vec<(u32, i32)> = columns.iter().map(|c| (c.ty, c.typmod)).collect();
                let derived = index_derived(info, table, &types)?;
                key.columns = (0..derived.exprs.len()).collect();
                key.types = derived.exprs.iter().map(|e| e.ty).collect();
                key.derived = Some(Box::new(derived));
                list.push(key);
                continue;
            }
            for &attnum in info.keys.iter().take(count) {
                let column = usize::try_from(attnum - 1)
                    .ok()
                    .and_then(|at| columns.get(at).map(|c| (at, c)));
                let Some((at, column)) = column else {
                    return Err(Error::internal("a unique index with a key that is not a column"));
                };
                if !key.shown.is_empty() {
                    key.shown.push_str(", ");
                }
                key.shown.push_str(&crate::ddl::quote_identifier(&column.name));
                key.columns.push(at);
                key.types.push(column.ty);
            }
            list.push(key);
        }
        Ok(list)
    }

    /// `checkInsertTargets`: the columns of the column list, or all the columns of the table when the statement has no list.
    pub(crate) fn insert_targets(
        &self,
        cols: &List,
        name: &str,
        columns: &[Column],
    ) -> Result<Vec<Given>> {
        if cols.is_empty() {
            return Ok((0..columns.len())
                .filter(|&i| !columns[i].is_dropped())
                .map(|i| (i, None, List::new()))
                .collect());
        }
        let mut given: Vec<Given> = Vec::with_capacity(cols.len());
        for col in cols {
            let Some(Node::ResTarget(rt)) = col else {
                return Err(Error::internal("an INSERT column that is not ResTarget"));
            };
            let at = place(rt.location);
            let col_name = rt.name.as_deref().unwrap_or_default();
            let Some(index) = columns.iter().position(|c| c.name == col_name) else {
                return Err(Error::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!("column \"{col_name}\" of relation \"{name}\" does not exist"),
                )
                .at_opt(at));
            };
            // A column can have more than one item only when each item sets a part of the column.
            let part = !rt.indirection.is_empty();
            if given.iter().any(|(i, _, other)| *i == index && (!part || other.is_empty())) {
                return Err(Error::new(
                    SqlState::DUPLICATE_COLUMN,
                    format!("column \"{col_name}\" specified more than once"),
                )
                .at_opt(at));
            }
            given.push((index, at, rt.indirection.clone()));
        }
        Ok(given)
    }

    /// `transformExpressionList` for a row of `VALUES` in an `INSERT`, which keeps `DEFAULT` at the top of an item. `t.*` and `(e).*` give an item for each column of the relation or each field of the row.
    pub(crate) fn value_items(&mut self, list: &List, kind: Kind) -> Result<Vec<Item>> {
        let mut items = Vec::with_capacity(list.len());
        for node in list {
            if let Some(Node::SetToDefault(d)) = node {
                items.push(Item::Default(place(d.location)));
                continue;
            }
            // `t.*` and `(e).*` give the columns of the relation or the fields of the row.
            let columns = self.with_kind(kind, |a| match node {
                Some(Node::ColumnRef(c)) => a.expand_star(c),
                Some(Node::A_Indirection(i)) => a.expand_indirection_star(i),
                _ => Ok(None),
            })?;
            if let Some(columns) = columns {
                items.extend(columns.into_iter().map(|(_, e)| Item::Expr(e)));
                continue;
            }
            items.push(Item::Expr(self.with_kind(kind, |a| a.transform(node.as_ref()))?));
        }
        Ok(items)
    }

    /// `transformInsertRow`: the items of a row, each with the type of its column, and `DEFAULT` as the default of the column. An identity column gets its default for `OVERRIDING USER VALUE`, and a column that is `GENERATED ALWAYS` takes only `DEFAULT` without `OVERRIDING SYSTEM VALUE`.
    pub(crate) fn insert_row(
        &mut self,
        items: Vec<Item>,
        table: u32,
        columns: &[Column],
        given: &[Given],
        explicit: bool,
        overriding: OverridingKind,
    ) -> Result<Vec<Expr>> {
        if items.len() > given.len() {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "INSERT has more expressions than target columns",
            )
            .at_opt(items[given.len()].place()));
        }
        if explicit && items.len() < given.len() {
            let mut error = Error::new(
                SqlState::SYNTAX_ERROR,
                "INSERT has more target columns than expressions",
            );
            if let [Item::Expr(expr)] = items.as_slice()
                && let ExprKind::Row(fields, _) = &expr.kind
                && fields.len() == given.len()
            {
                error = error.with_hint("The insertion source is a row expression containing the same number of columns expected by the INSERT. Did you accidentally use extra parentheses?");
            }
            return Err(error.at_opt(given[items.len()].1));
        }
        self.auto_view_given(items.len())?;
        // `transformInsertStmt` puts in `insertedCols` only the columns that have an expression, and `DEFAULT VALUES` gives no column a value.
        self.mark_inserted(&given[..items.len()]);
        let mut row = Vec::with_capacity(items.len());
        for (item, (index, at, indirection)) in items.into_iter().zip(given) {
            let (index, at) = (*index, *at);
            let column = &columns[index];
            // `rewriteTargetListIU`: the value of an identity column with `OVERRIDING`.
            let identity = self.identity(table, index);
            let item = match item {
                Item::Expr(_)
                    if identity != 0 && overriding == OverridingKind::OVERRIDING_USER_VALUE =>
                {
                    Item::Default(None)
                }
                Item::Expr(_)
                    if identity == b'a' && overriding == OverridingKind::OVERRIDING_NOT_SET =>
                {
                    return Err(Error::new(
                        SqlState::GENERATED_ALWAYS,
                        format!(
                            "cannot insert a non-DEFAULT value into column \"{}\"",
                            column.name
                        ),
                    )
                    .with_detail(format!(
                        "Column \"{}\" is an identity column defined as GENERATED ALWAYS.",
                        column.name
                    ))
                    .with_hint("Use OVERRIDING SYSTEM VALUE to override."));
                }
                Item::Expr(_) if self.generated(table, index) != 0 => {
                    return Err(Error::new(
                        SqlState::GENERATED_ALWAYS,
                        format!(
                            "cannot insert a non-DEFAULT value into column \"{}\"",
                            column.name
                        ),
                    )
                    .with_detail(format!("Column \"{}\" is a generated column.", column.name)));
                }
                item => item,
            };
            row.push(match item {
                Item::Default(_) if !indirection.is_empty() => {
                    return Err(default_part(indirection, at));
                }
                Item::Default(_) => self.column_default(table, index, column)?,
                Item::Expr(expr) if indirection.is_empty() => self.assign(expr, column)?,
                Item::Expr(expr) => self.assign_part(column, indirection, at, expr, None)?,
            });
        }
        Ok(row)
    }

    /// `transformAssignedExpr` for a column with subscripts: the value of the column `old` with `expr` in the part that `indirection` names. With no `old`, as in `INSERT`, the old value is a null value of the type of the column.
    ///
    /// # Errors
    ///
    /// The errors of [`Analyzer::assignment_indirection`].
    pub(crate) fn assign_part(
        &mut self,
        column: &Column,
        indirection: &List,
        at: Option<usize>,
        expr: Expr,
        old: Option<Expr>,
    ) -> Result<Expr> {
        let base = old.unwrap_or_else(|| {
            let mut null = Expr::new(ExprKind::Const(Value::Null), column.ty);
            null.typmod = column.typmod;
            null
        });
        let target = (base, &column.name[..], (column.ty, column.typmod));
        self.assignment_indirection(target, indirection, expr, Context::Assignment, at)
    }

    /// `attidentity` of the column of the table with the index `index` from 0, or 0 for a column that is not an identity column.
    pub(crate) fn identity(&self, table: u32, index: usize) -> u8 {
        let catalog = self.env.catalog();
        let relation = catalog.and_then(|c| c.relation(table));
        relation.and_then(|r| r.columns.get(index)).map_or(0, |c| c.identity)
    }

    /// `attgenerated` of the column of the table with the index `index` from 0, or 0 for a column that is not generated.
    pub(crate) fn generated(&self, table: u32, index: usize) -> u8 {
        let catalog = self.env.catalog();
        let relation = catalog.and_then(|c| c.relation(table));
        relation.and_then(|r| r.columns.get(index)).map_or(0, |c| c.generated)
    }

    /// The generated columns of the table, each with the index of the column from 0, its generation expression and `attgenerated`, as `ExecComputeStoredGenerated` computes them for a new row. The leaf partitions of a partitioned table compute their own columns.
    pub(crate) fn generated_columns(&mut self, table: u32) -> Result<Vec<Generated>> {
        let Some(catalog) = self.env.catalog() else { return Ok(Vec::new()) };
        let Some(relation) = catalog.relation(table) else { return Ok(Vec::new()) };
        if relation.kind == RelKind::Partitioned {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for (index, column) in relation.columns.iter().enumerate() {
            if column.generated == 0 || column.dropped {
                continue;
            }
            let attnum = i16::try_from(index + 1).unwrap_or(i16::MAX);
            if let Some(default) = catalog.default_of(table, attnum) {
                let mut expr: Expr = crate::node::read(&default.expr)?;
                // `ExecComputeStoredGenerated` sets `tts_tableOid` of the new row first, so `tableoid` is the table.
                with_table_oid(&mut expr, table);
                self.note_funcs(&expr);
                out.push((index, expr, column.generated));
            }
        }
        Ok(out)
    }

    /// `transformAssignedExpr`: the expression cast to the type of the column with an assignment cast.
    pub(crate) fn assign(&mut self, expr: Expr, column: &Column) -> Result<Expr> {
        let (from, at) = (expr.ty, expr.place());
        self.coerce_to_target(expr, column.ty, column.typmod, Context::Assignment, None)?
            .ok_or_else(|| {
                Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!(
                        "column \"{}\" is of type {} but expression is of type {}",
                        column.name,
                        types::name(column.ty),
                        types::name(from)
                    ),
                )
                .with_hint("You will need to rewrite or cast the expression.")
                .at_opt(at)
            })
    }

    /// `build_column_default`: the default of the column, or the default of its domain, cast to the type of the column. With no default, the value is a null value of the type, which a domain checks, as `coerce_null_to_domain` gives. The default of an identity column is the next value of its sequence, as `NextValueExpr` gives.
    pub(crate) fn column_default(
        &mut self,
        table: u32,
        index: usize,
        column: &Column,
    ) -> Result<Expr> {
        // A dropped column gets a null, as `ExecInitInsertProjection` gives it.
        if column.is_dropped() {
            return Ok(Expr::new(ExprKind::Const(Value::Null), 0));
        }
        // The write computes a generated column after the triggers `BEFORE`, which see a null.
        if self.generated(table, index) != 0 {
            // The default of a view is a value that the generated column of the table does not take.
            if self.auto_view_default(table, index).is_some() {
                return Err(Error::new(
                    SqlState::GENERATED_ALWAYS,
                    format!("cannot insert a non-DEFAULT value into column \"{}\"", column.name),
                )
                .with_detail(format!("Column \"{}\" is a generated column.", column.name)));
            }
            let mut null = Expr::new(ExprKind::Const(Value::Null), column.ty);
            null.typmod = column.typmod;
            return Ok(null);
        }
        let attnum = i16::try_from(index + 1).unwrap_or(i16::MAX);
        // A view of an automatically updatable view can have its own default for the column, which `rewriteTargetListIU` of the view gives.
        let view_default = self.auto_view_default(table, index);
        let sequence = self.env.catalog().and_then(|c| c.root_identity_sequence(table, attnum));
        if view_default.is_none()
            && self.identity(table, index) != 0
            && let Some(sequence) = sequence
        {
            let mut expr = nextval(sequence)?;
            // `NextValueExpr` does not check the privileges of the sequence.
            if let ExprKind::Func(func) = &mut expr.kind {
                func.args.push(Expr::constant(Value::Bool(true), oid::BOOL, -1, None));
            }
            return self
                .coerce_to_target(expr, column.ty, column.typmod, Context::Assignment, None)?
                .ok_or_else(|| {
                    Error::internal("an identity column of a type that is not an integer")
                });
        }
        let catalog = self.env.catalog();
        let (table, attnum) = view_default.unwrap_or((table, attnum));
        let text = catalog.and_then(|c| c.default_of(table, attnum)).map(|d| d.expr.clone());
        let text = text.or_else(|| {
            catalog
                .and_then(|c| c.type_by_oid(column.ty))
                .and_then(|ty| ty.domain.as_ref())
                .and_then(|d| d.default.clone())
        });
        let Some(text) = text else {
            let null = Expr::new(ExprKind::Const(Value::Null), oid::UNKNOWN);
            let mut expr = self
                .coerce_to_target(null, column.ty, column.typmod, Context::Assignment, None)?
                .unwrap_or_else(|| Expr::new(ExprKind::Const(Value::Null), column.ty));
            if let ExprKind::Const(_) = expr.kind {
                expr.typmod = column.typmod;
            }
            return Ok(expr);
        };
        let expr = crate::node::read(&text)?;
        self.note_funcs(&expr);
        let from = expr.ty;
        self.coerce_to_target(expr, column.ty, column.typmod, Context::Assignment, None)?
            .ok_or_else(|| {
                Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!(
                        "column \"{}\" is of type {} but default expression is of type {}",
                        column.name,
                        types::name(column.ty),
                        types::name(from)
                    ),
                )
                .with_hint("You will need to rewrite or cast the expression.")
            })
    }

    /// The row of the table: the expression of each column that the statement gives, and the default of each other column.
    pub(crate) fn full_row(
        &mut self,
        table: u32,
        columns: &[Column],
        given: &[Given],
        exprs: Vec<Expr>,
    ) -> Result<Vec<Expr>> {
        let mut slots: Vec<Option<Expr>> = vec![None; columns.len()];
        for (expr, &(index, _, _)) in exprs.into_iter().zip(given) {
            // `process_matched_tle`: the items that set parts of the same column make one value.
            slots[index] = Some(match slots[index].take() {
                Some(prior) => merge_parts(prior, expr, &columns[index].name)?,
                None => expr,
            });
        }
        let mut row = Vec::with_capacity(columns.len());
        for (index, slot) in slots.into_iter().enumerate() {
            row.push(match slot {
                Some(expr) => expr,
                None => self.column_default(table, index, &columns[index])?,
            });
        }
        Ok(row)
    }

    /// `INSERT ... SELECT`: the query of the `SELECT` is a subquery, and the row reads its columns. A column of type `unknown` that is a constant or a parameter comes as it is, so that the cast to the type of the column can use its text.
    fn insert_select(
        &mut self,
        select: &SelectStmt,
        table: u32,
        columns: &[Column],
        given: &[Given],
        explicit: bool,
        overriding: OverridingKind,
    ) -> Result<Query> {
        self.keep_unknowns = true;
        let sub = self.sub_select(select, false);
        self.keep_unknowns = false;
        let sub = sub?;
        let shown: Vec<&Target> = sub.targets.iter().filter(|t| !t.junk).collect();
        let items: Vec<Item> = shown
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let unknown = t.expr.ty == oid::UNKNOWN
                    && matches!(t.expr.kind, ExprKind::Const(_) | ExprKind::Param(_));
                if unknown {
                    return Item::Expr(t.expr.clone());
                }
                let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
                let var = Var { relation: 0, attnum, levels_up: 0 };
                let mut expr = Expr::new(ExprKind::Var(var), t.expr.ty);
                expr.typmod = t.expr.typmod;
                expr.location = t.expr.place();
                Item::Expr(expr)
            })
            .collect();
        let sub_columns: Vec<Column> = shown
            .iter()
            .map(|t| Column {
                name: t.name.clone(),
                ty: t.expr.ty,
                typmod: t.expr.typmod,
                not_null: false,
            })
            .collect();
        let exprs = self.insert_row(items, table, columns, given, explicit, overriding)?;
        let row = self.full_row(table, columns, given, exprs)?;
        let mut relation = Relation::new(0, sub_columns, Some(Box::new(sub)), None, None);
        relation.name = "*SELECT*".to_string();
        Ok(Query {
            relations: vec![relation],
            from: vec![FromItem::Relation(0)],
            targets: targets(columns, row),
            ..Query::default()
        })
    }

    /// A row of `VALUES` as an [`InsertDef`] keeps it: the value of each item for its column, as `transformInsertRow` gives it, or `None` for `DEFAULT`. The value for a part of a column is the assignment to the part of a null value. The row keeps the value of an identity column with `OVERRIDING USER VALUE`, which only `rewriteTargetListIU` drops.
    pub(crate) fn kept_row(
        &mut self,
        items: &[Item],
        columns: &[Column],
        given: &[Given],
    ) -> Result<Vec<Option<Expr>>> {
        items
            .iter()
            .zip(given)
            .map(|(item, (index, at, indirection))| match item {
                Item::Default(_) => Ok(None),
                Item::Expr(expr) if indirection.is_empty() => {
                    self.assign(expr.clone(), &columns[*index]).map(Some)
                }
                Item::Expr(expr) => self
                    .assign_part(&columns[*index], indirection, *at, expr.clone(), None)
                    .map(Some),
            })
            .collect()
    }

    /// `INSERT ... VALUES` with more than one row: each row gets the types of the columns, and the rows are a `VALUES` relation that the row reads.
    fn insert_values(
        &mut self,
        select: &SelectStmt,
        table: u32,
        columns: &[Column],
        (given, explicit, overriding): (&[Given], bool, OverridingKind),
        mut kept: Option<&mut Vec<Vec<Option<Expr>>>>,
    ) -> Result<Query> {
        let mut rows: Vec<Vec<Expr>> = Vec::with_capacity(select.valuesLists.len());
        let mut length = None;
        for list in &select.valuesLists {
            let Some(Node::List(list)) = list else {
                return Err(Error::internal("a VALUES row that is not a list"));
            };
            let items = self.value_items(list, Kind::Values)?;
            match length {
                None => length = Some(items.len()),
                Some(n) if n != items.len() => {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        "VALUES lists must all be the same length",
                    )
                    .at_opt(first_place(&items)));
                }
                Some(_) => {}
            }
            if let Some(kept) = kept.as_mut() {
                kept.push(self.kept_row(&items, columns, given)?);
            }
            let row = self.insert_row(items, table, columns, given, explicit, overriding)?;
            // `transformInsertRow` with `strip_indirection`: the row keeps the value of a part of a column, and the row of the table sets the part.
            let row = row
                .into_iter()
                .zip(given)
                .map(
                    |(expr, (_, _, indirection))| {
                        if indirection.is_empty() { expr } else { strip_part(expr) }
                    },
                )
                .collect();
            rows.push(row);
        }
        let width = rows.first().map_or(0, Vec::len);
        let value_columns: Vec<Column> = given
            .iter()
            .take(width)
            .enumerate()
            .map(|(i, (index, _, indirection))| {
                let (ty, typmod) = match rows.first().and_then(|row| row.get(i)) {
                    Some(expr) if !indirection.is_empty() => (expr.ty, expr.typmod),
                    _ => (columns[*index].ty, columns[*index].typmod),
                };
                Column { name: format!("column{}", i + 1), ty, typmod, not_null: false }
            })
            .collect();
        let mut exprs = Vec::with_capacity(value_columns.len());
        for (i, (c, (index, at, indirection))) in value_columns.iter().zip(given).enumerate() {
            let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
            let mut expr =
                Expr::new(ExprKind::Var(Var { relation: 0, attnum, levels_up: 0 }), c.ty);
            expr.typmod = c.typmod;
            if !indirection.is_empty() {
                expr = self.assign_part(&columns[*index], indirection, *at, expr, None)?;
            }
            exprs.push(expr);
        }
        let row = self.full_row(table, columns, given, exprs)?;
        let mut relation = Relation::new(0, value_columns, None, None, Some(rows));
        relation.name = "*VALUES*".to_string();
        Ok(Query {
            relations: vec![relation],
            from: vec![FromItem::Relation(0)],
            targets: targets(columns, row),
            ..Query::default()
        })
    }

    /// The check constraints of the table with their conditions, in the order of their names, as `CheckConstraintFetch` sorts them.
    pub(crate) fn checks(&mut self, table: u32) -> Result<Vec<(String, Expr)>> {
        let Some(catalog) = self.env.catalog() else { return Ok(Vec::new()) };
        let mut checks: Vec<(String, &str)> = catalog
            .constraints_of(table)
            .filter(|c| c.kind == ConKind::Check && c.enforced)
            .map(|c| (c.name.clone(), c.expr.as_deref().unwrap_or_default()))
            .collect();
        checks.sort_by(|a, b| a.0.cmp(&b.0));
        // `ExecConstraints` tests the row with `tts_tableOid` set, so `tableoid` is the table.
        let checks: Vec<(String, Expr)> = checks
            .into_iter()
            .map(|(name, text)| {
                let mut expr: Expr = crate::node::read(text)?;
                with_table_oid(&mut expr, table);
                Ok((name, expr))
            })
            .collect::<Result<_>>()?;
        for (_, expr) in &checks {
            self.note_funcs(expr);
        }
        Ok(checks)
    }
}

/// The expression with the oid of the table in the place of each `tableoid` of the row, which is the table that gets the row.
fn with_table_oid(expr: &mut Expr, table: u32) {
    expr.each_mut(0, &mut |e, depth| {
        let ExprKind::Var(var) = e.kind else { return false };
        if var.levels_up != depth || var.attnum != TABLE_OID_ATTNUM {
            return false;
        }
        *e = Expr::constant(Value::Oid(table), oid::OID, -1, None);
        true
    });
}

/// The default of a `serial` column or of an identity column before the cast to the type of the column: `nextval('sequence'::regclass)`.
pub(crate) fn nextval(sequence: u32) -> Result<Expr> {
    let proc = builtin::procs_named("nextval")
        .next()
        .ok_or_else(|| Error::internal("function nextval does not exist"))?;
    let arg = Expr::constant(Value::Oid(sequence), oid::REGCLASS, -1, None);
    let func = Func {
        oid: proc.oid,
        args: vec![arg],
        form: FuncForm::Call,
        variadic: false,
        retset: false,
    };
    Ok(Expr::new(ExprKind::Func(func), proc.rettype))
}

/// `isGeneralSelect`: a source that is not a plain `VALUES` list runs as a subquery.
fn general_select(s: &SelectStmt) -> bool {
    s.valuesLists.is_empty()
        || !s.sortClause.is_empty()
        || s.limitOffset.is_some()
        || s.limitCount.is_some()
        || !s.lockingClause.is_empty()
        || s.withClause.is_some()
}

/// The targets of the row, with the names of the columns.
pub(crate) fn targets(columns: &[Column], row: Vec<Expr>) -> Vec<Target> {
    columns
        .iter()
        .zip(row)
        .map(|(c, expr)| Target { name: c.name.clone(), expr, origin: None, junk: false })
        .collect()
}

/// The query of a row with no `FROM`.
fn row_query(columns: &[Column], row: Vec<Expr>) -> Query {
    Query { targets: targets(columns, row), ..Query::default() }
}

/// The arbiters of `ON CONFLICT` that the analysis finds, which the planner matches with the unique indexes.
enum Inference {
    /// No inference: all the unique indexes.
    All,
    /// The set of the columns of an inference, from 0, in order, the expressions of the inference, the parts of its predicate, and the elements with a collation or an operator class.
    Columns { columns: Vec<usize>, exprs: Vec<Expr>, predicate: Vec<Expr>, exact: Vec<ExactElem> },
    /// The OID of the index of the constraint that `ON CONSTRAINT` names, or 0 for a constraint with no index.
    Constraint(u32),
}

/// `ON CONFLICT` and `RETURNING` of an `INSERT`, and `ON CONFLICT` in the form that [`ConflictDef`] keeps.
type InsertClauses = (Option<OnConflict>, Option<Box<Query>>, Option<ConflictDef>, Option<usize>);

/// The relations of the query of `DO UPDATE` or of `DO SELECT` for [`ConflictDef::relations`], where the relation at `excluded` has the name `excluded`, as its entry has the alias.
fn excluded_named(relations: &[Relation], excluded: usize) -> Vec<Relation> {
    let mut relations = relations.to_vec();
    if let Some(relation) = relations.get_mut(excluded) {
        relation.name = "excluded".to_string();
        relation.alias = Some(Vec::new());
    }
    relations
}

/// An element of an inference with a collation or an operator class, `InferenceElem` with `infercollid` or `inferopclass`.
struct ExactElem {
    target: Exact,
    /// The collation, or 0.
    collation: u32,
    /// The operator family and the input type of the operator class.
    class: Option<(u32, u32)>,
}

/// The column or the expression of an [`ExactElem`].
#[derive(Clone, Copy)]
enum Exact {
    /// The column number in the table.
    Column(i16),
    /// The place of the expression in the expressions of the inference.
    Expr(usize),
}

/// `infer_collation_opclass_match`: true when a key column of the index is the element with its collation and the family and the input type of its operator class.
fn exact_match(
    elem: &ExactElem,
    exprs: &[Expr],
    key: &Unique,
    catalog: Option<&rupg_catalog::Catalog>,
) -> bool {
    let info = catalog.and_then(|c| c.relation(key.oid)).and_then(|rel| rel.index.as_ref());
    let Some(info) = info else { return false };
    let count = usize::try_from(info.key_count).unwrap_or_default();
    for (natt, &attno) in info.keys.iter().take(count).enumerate() {
        if let Some(class) = elem.class {
            let oid = info.classes.get(natt).copied().unwrap_or_default();
            let user = catalog.and_then(|c| c.opclass(oid)).map(|c| (c.family, c.intype));
            let found = user.or_else(|| {
                builtin::opclasses().iter().find(|c| c.oid == oid).map(|c| (c.family, c.intype))
            });
            if found != Some(class) {
                continue;
            }
        }
        if elem.collation != 0 && info.collations.get(natt) != Some(&elem.collation) {
            continue;
        }
        match elem.target {
            Exact::Column(attnum) if attno == attnum => return true,
            Exact::Expr(at) if attno == 0 => {
                let indexed = key.derived.as_ref().and_then(|d| d.exprs.get(natt));
                if indexed.is_some_and(|i| exprs.get(at).is_some_and(|e| e.same(i))) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// `infer_arbiter_indexes`: the places in `unique` of the arbiter indexes. An inference takes each unique index whose key has the same set of columns. An index that is not ready is not an arbiter, and an index of a partitioned table that is not valid is not an arbiter, because a partition can be without it.
fn arbiters(
    inference: Inference,
    unique: &[Unique],
    catalog: Option<&rupg_catalog::Catalog>,
    partitioned: bool,
) -> Result<Vec<usize>> {
    let usable = |key: &Unique| {
        let info = catalog.and_then(|c| c.relation(key.oid)).and_then(|rel| rel.index.as_ref());
        info.is_none_or(|i| i.ready && (i.valid || !partitioned))
    };
    let places: Vec<usize> = match inference {
        Inference::All => return Ok((0..unique.len()).collect()),
        Inference::Constraint(0) => {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                "constraint in ON CONFLICT clause has no associated index",
            ));
        }
        Inference::Constraint(index) => {
            let places: Vec<usize> = unique
                .iter()
                .enumerate()
                .filter(|(_, key)| key.oid == index)
                .map(|(at, _)| at)
                .collect();
            // An empty list is the index of an exclusion constraint.
            return Ok(places);
        }
        Inference::Columns { columns: set, exprs, predicate, exact } => unique
            .iter()
            .enumerate()
            .filter(|(_, key)| usable(key))
            .filter(|(_, key)| exact.iter().all(|e| exact_match(e, &exprs, key, catalog)))
            .filter(|(_, key)| {
                let Some(derived) = &key.derived else {
                    let mut key = key.columns.clone();
                    key.sort_unstable();
                    key.dedup();
                    return key == set && exprs.is_empty();
                };
                // The columns of the key must be the columns of the inference, and each expression of the key must be an expression of the inference and the other way around.
                let mut columns = Vec::new();
                let mut indexed = Vec::new();
                for expr in &derived.exprs {
                    match expr.kind {
                        ExprKind::Var(var) => columns.extend(usize::try_from(var.attnum - 1).ok()),
                        _ => indexed.push(expr),
                    }
                }
                columns.sort_unstable();
                columns.dedup();
                // `predicate_implied_by`: each part of the predicate of the index is a part of the predicate of the inference.
                // An expression with a collation or an operator class matched in `exact_match`.
                let checked =
                    |at: usize| exact.iter().any(|e| matches!(e.target, Exact::Expr(x) if x == at));
                columns == set
                    && exprs
                        .iter()
                        .enumerate()
                        .all(|(at, e)| checked(at) || indexed.iter().any(|i| i.same(e)))
                    && indexed.iter().all(|i| exprs.iter().any(|e| e.same(i)))
                    && derived.predicate.iter().all(|p| predicate.iter().any(|e| e.same(p)))
            })
            .map(|(at, _)| at)
            .collect(),
    };
    // At least one arbiter must be valid, because an index that is not valid can be without some rows.
    let valid = |at: &usize| {
        let info =
            catalog.and_then(|c| c.relation(unique[*at].oid)).and_then(|rel| rel.index.as_ref());
        info.is_none_or(|i| i.valid)
    };
    if !places.iter().any(valid) {
        return Err(Error::new(
            SqlState::INVALID_COLUMN_REFERENCE,
            "there is no unique or exclusion constraint matching the ON CONFLICT specification",
        ));
    }
    Ok(places)
}

/// The columns of the target table of a write. A dropped column keeps its place with no name and the type 0, and each new row has a null for it.
pub(crate) fn write_columns(rel: &rupg_catalog::Relation) -> Vec<Column> {
    rel.columns
        .iter()
        .map(|c| Column {
            name: if c.dropped { String::new() } else { c.name.clone() },
            ty: c.ty,
            typmod: c.typmod,
            not_null: c.not_null && !c.not_null_invalid,
        })
        .collect()
}

/// True for a trigger of the table `table` that the user made and that fires in the mode of `session_replication_role`, which is `replica` when `replica` is true. The session checks the foreign keys itself.
fn user_trigger(t: &rupg_catalog::Trigger, table: u32, replica: bool) -> bool {
    t.relation == table && !t.internal && t.fires(replica)
}

/// The bit of `tgtype` for the event of a write.
pub(crate) fn trigger_event(kind: WriteKind) -> i16 {
    match kind {
        WriteKind::Insert => rupg_catalog::TRIGGER_INSERT,
        WriteKind::Update => rupg_catalog::TRIGGER_UPDATE,
        WriteKind::Delete => rupg_catalog::TRIGGER_DELETE,
        WriteKind::Merge => {
            rupg_catalog::TRIGGER_INSERT
                | rupg_catalog::TRIGGER_UPDATE
                | rupg_catalog::TRIGGER_DELETE
        }
    }
}

/// The query that reads the rows of `rows` as a subquery with the name of the table, and gives the columns and then the condition of each check constraint. The values of `tail` come after the columns in each row of `rows`, and after the conditions in each row of the query, as the carried values of `RETURNING` and the ID of the version of the old row of an `UPDATE`. A stored generated column of `generated` gets the value of its generation expression on the row, as `ExecComputeStoredGenerated` gives it before `ExecConstraints`, and the checks see that value.
pub(crate) fn with_checks(
    name: &str,
    columns: &[Column],
    rows: Query,
    checks: Vec<(String, Expr)>,
    generated: &[Generated],
    tail: &[Column],
) -> Query {
    let width = columns.len();
    let mut columns = columns.to_vec();
    columns.extend_from_slice(tail);
    let mut targets: Vec<Target> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
            let mut expr =
                Expr::new(ExprKind::Var(Var { relation: 0, attnum, levels_up: 0 }), c.ty);
            expr.typmod = c.typmod;
            Target { name: c.name.clone(), expr, origin: None, junk: false }
        })
        .collect();
    let tail = targets.split_off(width);
    // A virtual generated column keeps its null, and a read of the row computes it.
    for (index, expr, kind) in generated {
        if *kind == b's'
            && let Some(target) = targets.get_mut(*index)
        {
            target.expr = expr.clone();
        }
    }
    targets.extend(checks.into_iter().map(|(n, mut expr)| {
        compute_generated(&mut expr, generated, &columns[..width]);
        Target { name: n, expr, origin: None, junk: false }
    }));
    targets.extend(tail);
    let mut relation = Relation::new(0, columns, Some(Box::new(rows)), None, None);
    relation.name = name.to_string();
    Query {
        relations: vec![relation],
        from: vec![FromItem::Relation(0)],
        targets,
        ..Query::default()
    }
}

/// The expression with the generation expression of each stored generated column of `generated` in place of the column of the relation 0. The whole row of the relation 0 becomes a `RowExpr` of the `columns`, with the generation expressions in it.
fn compute_generated(expr: &mut Expr, generated: &[Generated], columns: &[Column]) {
    if generated.is_empty() {
        return;
    }
    expr.each_mut(0, &mut |e, depth| {
        let ExprKind::Var(var) = e.kind else { return false };
        if var.levels_up != depth || var.relation != 0 {
            return false;
        }
        if var.attnum == 0 {
            // A dropped column has no field.
            let fields = columns.iter().enumerate().filter(|(_, c)| c.ty != 0);
            let fields = fields
                .map(|(i, c)| match generated.iter().find(|(at, ..)| *at == i) {
                    Some((_, value, _)) => {
                        let mut value = value.clone();
                        value.raise(depth);
                        value
                    }
                    None => {
                        let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
                        let var = Var { relation: 0, attnum, levels_up: depth };
                        let mut field = Expr::new(ExprKind::Var(var), c.ty);
                        field.typmod = c.typmod;
                        field
                    }
                })
                .collect();
            e.kind = ExprKind::Row(fields, CastForm::Implicit);
            return true;
        }
        let index = usize::try_from(var.attnum).ok().and_then(|n| n.checked_sub(1));
        let Some((_, value, _)) = generated.iter().find(|(i, ..)| Some(*i) == index) else {
            return false;
        };
        let mut value = value.clone();
        value.raise(depth);
        *e = value;
        true
    });
}

/// The error of `DEFAULT` for a part of a column, which has no default.
pub(crate) fn default_part(indirection: &List, at: Option<usize>) -> Error {
    let what = match indirection.first() {
        Some(Some(Node::A_Indices(_))) => "an array element",
        _ => "a subfield",
    };
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("cannot set {what} to DEFAULT")).at_opt(at)
}

/// The value that an assignment to a part of a column sets, without the assignments of each level.
fn strip_part(expr: Expr) -> Expr {
    match expr.kind {
        ExprKind::CoerceToDomain(arg, _) => strip_part(*arg),
        ExprKind::Subscript(sub) if sub.assign.is_some() => {
            sub.assign.map_or_else(|| Expr::new(ExprKind::Const(Value::Null), 0), strip_part)
        }
        ExprKind::FieldStore(store) if store.fields.len() == 1 => {
            store.fields.into_iter().next().map_or_else(
                || Expr::new(ExprKind::Const(Value::Null), 0),
                |(_, value)| strip_part(value),
            )
        }
        _ => expr,
    }
}

/// Whether the value is an assignment to a part of a column.
fn is_part(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Subscript(sub) => sub.assign.is_some(),
        ExprKind::FieldStore(_) => true,
        _ => false,
    }
}

/// `process_matched_tle`: two assignments to parts of the same column as one assignment, in which `prior` gives the old value for `src`. When both check the same domain, the domain checks the value after both.
///
/// # Errors
///
/// `multiple assignments to same column` when one of the two sets the whole column.
pub(crate) fn merge_parts(prior: Expr, mut src: Expr, name: &str) -> Result<Expr> {
    let domain = |e: &Expr| matches!(e.kind, ExprKind::CoerceToDomain(..));
    if prior.ty == src.ty && domain(&prior) && domain(&src) {
        let (ExprKind::CoerceToDomain(prior, _), ExprKind::CoerceToDomain(arg, _)) =
            (prior.kind, &mut src.kind)
        else {
            return Err(Error::internal("an assignment to a domain that is not CoerceToDomain"));
        };
        let inner = std::mem::replace(&mut **arg, Expr::new(ExprKind::Const(Value::Null), 0));
        **arg = merge_parts(*prior, inner, name)?;
        return Ok(src);
    }
    if prior.ty != src.ty || !is_part(&prior) || !is_part(&src) {
        return Err(Error::new(
            SqlState::SYNTAX_ERROR,
            format!("multiple assignments to same column \"{name}\""),
        ));
    }
    match (&mut src.kind, prior.kind) {
        // Two assignments to fields are one `FieldStore` with the fields of both.
        (ExprKind::FieldStore(store), ExprKind::FieldStore(old)) => {
            let FieldStore { arg, mut fields, .. } = *old;
            fields.append(&mut store.fields);
            store.fields = fields;
            store.arg = arg;
        }
        (ExprKind::FieldStore(store), kind) => store.arg = Expr { kind, ..prior },
        (ExprKind::Subscript(sub), kind) => sub.container = Expr { kind, ..prior },
        _ => {}
    }
    Ok(src)
}
