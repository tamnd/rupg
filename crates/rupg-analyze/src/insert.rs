//! `transformInsertStmt` of `analyze.c`, with `checkInsertTargets` and `transformAssignedExpr` of `parse_target.c` and the defaults of `rewriteTargetListIU` and `expand_insert_targetlist`.
//!
//! The query of an `INSERT` gives the full row of the table for each new row, in the order of the attributes. A column that the statement does not give gets its default, the default of its domain or a null value. When the table has check constraints, the query reads the rows as a subquery and its targets after the columns are the conditions of the constraints in the order of their names, which the executor tests.

use rupg_catalog::{ConKind, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::{
    InsertStmt, List, Node, OnConflictAction, OnConflictClause, OverridingKind, RangeVar,
    SelectStmt,
};
use rupg_types::{Value, oid};

use crate::Analyzer;
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::expr::{Expr, ExprKind, FieldStore, Func, FuncForm, Var};
use crate::from::{Column, FromItem, Relation};
use crate::merge::Merge;
use crate::modify::{carried_targets, set_columns};
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
    /// The number of columns of the table. The first targets of the query are the columns. For a `DELETE` it is 0.
    pub width: usize,
    /// The names of the check constraints in the order of their names. The targets after the columns are the conditions of these constraints, in the same order.
    pub checks: Vec<String>,
    /// The unique indexes of the table in the order of their OIDs, as `RelationGetIndexList` gives them. Each new version of a row must have a key that no other version has.
    pub unique: Vec<Unique>,
    /// The query of `RETURNING`, which reads one relation: the rows that the statement writes. Each row has the columns of the table, then the values that the query of the statement carries for it. For an `UPDATE` and a `DELETE` the carried values come after the columns and the conditions in each row of the query of the statement, before the ID of the version.
    pub returning: Option<Box<Query>>,
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
}

/// `OnConflictExpr`: what an `INSERT` does for a new row with the key of a row that the table has.
#[derive(Clone, Debug, PartialEq)]
pub struct OnConflict {
    /// The places in [`Write::unique`] of the arbiter indexes, which `infer_arbiter_indexes` finds. A conflict in another unique index is an error.
    pub arbiters: Vec<usize>,
    /// True for a clause with no inference. Each unique index is then an arbiter, also an index of a partition that the partitioned table does not have.
    pub all: bool,
    /// The query of `DO UPDATE`, or `None` for `DO NOTHING`. It reads two relations: the row that the table has, with the ID of its version at the end, and `EXCLUDED`, the row that the statement proposed. It gives the new row, then the conditions of the check constraints, then the values that `RETURNING` carries, or no row when the condition of `WHERE` is not true.
    pub update: Option<Box<Query>>,
    /// The number of values that `RETURNING` carries: the columns of the old row. A row that the statement inserts has nulls for them.
    pub carried: usize,
    /// The foreign keys that an update of a row checks, in the order of the names of their triggers.
    pub foreign: Vec<Foreign>,
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
    /// The OID of the constraint, which `SET CONSTRAINTS` names.
    pub constraint: u32,
    /// `tgdeferrable`: `SET CONSTRAINTS` can defer the trigger. The trigger of an action is not deferrable.
    pub deferrable: bool,
    /// `tginitdeferred`: the trigger fires at the commit when `SET CONSTRAINTS` does not change its mode.
    pub deferred: bool,
    /// `tgisclone`: the trigger is a clone of a trigger of the parent table. The delete of a row that an `UPDATE` moves to another partition skips such a trigger of the referenced table, because the event of the update of the partitioned table does its work.
    pub cloned: bool,
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
}

/// `quoteOneName` of `ri_triggers.c`: a name in double quotes, with each double quote doubled.
fn quote_one(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The statement of an action of a foreign key on the referencing table `table`, as `RI_FKey_cascade_del`, `RI_FKey_cascade_upd` and `ri_set` make it. `names` are the key columns of the referencing table.
fn action_query(
    (action, kind): (Action, WriteKind),
    (table, only): (&str, bool),
    names: &[&str],
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
            let set: Vec<String> = columns.iter().map(|c| format!(" {c} = {value}")).collect();
            format!("UPDATE {only}{table} SET{}", set.join(","))
        }
    };
    // The parameters of the old key come after the new key for `ON UPDATE CASCADE`.
    let first =
        if action == Action::Cascade && kind == WriteKind::Update { names.len() } else { 0 };
    for (at, column) in columns.iter().enumerate() {
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

/// A column that the statement gives: the index of the column in the table, the place of its name in the column list, and the subscripts after the name, which are empty when the item sets the whole column.
pub(crate) type Given = (usize, Option<usize>, List);

/// The bits in `tgtype` of a trigger `INSTEAD OF ... FOR EACH ROW`.
const INSTEAD_ROW: i16 = rupg_catalog::TRIGGER_ROW | rupg_catalog::TRIGGER_INSTEAD;

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
        self.write_with(s.withClause.as_deref())?;
        let rv = s.relation.as_deref().ok_or_else(|| Error::internal("an INSERT with no table"))?;
        let (table, name, columns) = self.target_table(rv, WriteKind::Insert)?;
        let given = self.insert_targets(&s.cols, &name, &columns)?;
        let explicit = !s.cols.is_empty();
        let select = match &s.selectStmt {
            None => None,
            Some(Node::SelectStmt(select)) => Some(&**select),
            Some(_) => return Err(Error::internal("an INSERT source that is not SELECT")),
        };
        let mut query = match select {
            None => {
                let row = self.full_row(table, &columns, &given, Vec::new())?;
                row_query(&columns, row)
            }
            Some(select) if general_select(select) => {
                self.insert_select(select, table, &columns, &given, explicit, s.r#override)?
            }
            Some(select) if select.valuesLists.len() > 1 => {
                self.insert_values(select, table, &columns, &given, explicit, s.r#override)?
            }
            Some(select) => {
                let Some(Some(Node::List(list))) = select.valuesLists.first() else {
                    return Err(Error::internal("a VALUES row that is not a list"));
                };
                let items = self.value_items(list, Kind::ValuesSingle)?;
                let exprs =
                    self.insert_row(items, table, &columns, &given, explicit, s.r#override)?;
                let row = self.full_row(table, &columns, &given, exprs)?;
                row_query(&columns, row)
            }
        };
        // `ExecFindPartition` routes each row, and the partition then checks it.
        let checks = if self.is_partitioned(table) { Vec::new() } else { self.checks(table)? };
        let names = checks.iter().map(|(n, _)| n.clone()).collect();
        let conflict_set = s
            .onConflictClause
            .as_deref()
            .filter(|c| matches!(c.action, OnConflictAction::ONCONFLICT_UPDATE))
            .map(|c| &c.targetList);
        let mut recheck = self.recheck(table, WriteKind::Insert, &name, &columns, &checks);
        if recheck.is_none() && conflict_set.is_some() {
            // The triggers `BEFORE UPDATE ... FOR EACH ROW` of `DO UPDATE` also come before the checks.
            recheck = self.recheck(table, WriteKind::Update, &name, &columns, &checks);
        }
        let updated = conflict_set.map_or_else(Vec::new, |list| set_columns(list, &columns));
        if !checks.is_empty() {
            query = with_checks(&name, &columns, query, checks, &[]);
        }
        let width = columns.len();
        let unique = self.unique_keys(table, &columns)?;
        let foreign = self.foreign_keys(table, WriteKind::Insert, &columns)?;
        let (mut on_conflict, mut returning) = (None, None);
        if s.onConflictClause.is_some() || s.returningClause.is_some() {
            let outer = std::mem::take(&mut self.scope);
            // `ON CONFLICT` and `RETURNING` can read the queries of `WITH`.
            self.scope.ctes.clone_from(&outer.ctes);
            let result = self.insert_clauses(s, rv, table, &name, &columns, &unique);
            self.scope = outer;
            (on_conflict, returning) = result?;
        }
        query.write = Some(Write {
            kind: WriteKind::Insert,
            table,
            name,
            width,
            checks: names,
            unique,
            returning,
            foreign,
            on_conflict,
            merge: None,
            recheck,
            updated,
            instead: self.is_view(table),
            inherit: false,
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
    ) -> Result<(Option<OnConflict>, Option<Box<Query>>)> {
        self.target_from(rv, &[])?;
        let (mut on_conflict, inference) = match s.onConflictClause.as_deref() {
            None => (None, Inference::All),
            Some(clause) => {
                let (on_conflict, inference) =
                    self.on_conflict(clause, rv, table, name, columns)?;
                (Some(on_conflict), inference)
            }
        };
        // With `DO UPDATE`, the old row of `RETURNING` is the row that the statement updates, as for an `UPDATE`.
        let update = on_conflict.as_ref().is_some_and(|c| c.update.is_some());
        let kind = if update { WriteKind::Update } else { WriteKind::Insert };
        let (returning, carried) = match s.returningClause.as_deref() {
            None => (None, Vec::new()),
            Some(clause) => {
                let (query, carried) = self.returning(clause, kind)?;
                (Some(Box::new(query)), carried)
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
            // The leaf partition checks the new row of a partitioned table.
            let checks = if self.is_partitioned(table) { Vec::new() } else { self.checks(table)? };
            if !checks.is_empty() {
                query = Box::new(with_checks(name, columns, *query, checks, &tail));
            }
            conflict.update = Some(query);
        }
        // The planner matches the inference with the indexes after the analysis.
        if let Some(conflict) = &mut on_conflict {
            conflict.all = matches!(inference, Inference::All);
            conflict.arbiters = arbiters(inference, unique)?;
        }
        Ok((on_conflict, returning))
    }

    /// `transformOnConflictClause`: the inference of the arbiter indexes, and the query of `DO UPDATE` with the new row. `SET` and `WHERE` see the target table and `EXCLUDED`, and the clauses after them see only the target table.
    fn on_conflict(
        &mut self,
        clause: &OnConflictClause,
        rv: &RangeVar,
        table: u32,
        name: &str,
        columns: &[Column],
    ) -> Result<(OnConflict, Inference)> {
        let update = match clause.action {
            OnConflictAction::ONCONFLICT_NOTHING => false,
            OnConflictAction::ONCONFLICT_UPDATE => true,
            _ => return Err(not_yet("ON CONFLICT DO SELECT")),
        };
        let inference = self.inference(clause, update, table, name, columns)?;
        let mut on_conflict = OnConflict {
            arbiters: Vec::new(),
            all: false,
            update: None,
            carried: 0,
            foreign: Vec::new(),
        };
        if !update {
            return Ok((on_conflict, inference));
        }
        let excluded = self.add_excluded()?;
        let parts = self.update_row(rv, table, name, columns, &clause.targetList).and_then(|row| {
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
        on_conflict.update = Some(Box::new(query));
        on_conflict.foreign = self.foreign_keys(table, WriteKind::Update, columns)?;
        Ok((on_conflict, inference))
    }

    /// `transformOnConflictArbiter`: the columns of the inference, or the index of the constraint that the clause names.
    fn inference(
        &mut self,
        clause: &OnConflictClause,
        update: bool,
        table: u32,
        name: &str,
        columns: &[Column],
    ) -> Result<Inference> {
        let Some(infer) = clause.infer.as_deref() else {
            if update {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "ON CONFLICT DO UPDATE requires inference specification or constraint name",
                )
                .with_hint("For example, ON CONFLICT (column_name).")
                .at_opt(place(clause.location)));
            }
            return Ok(Inference::All);
        };
        if let Some(conname) = infer.conname.as_deref() {
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
            return Ok(Inference::Constraint(constraint.index));
        }
        let mut set = Vec::with_capacity(infer.indexElems.len());
        for node in &infer.indexElems {
            let Some(Node::IndexElem(elem)) = node else {
                return Err(Error::internal("an inference element that is not IndexElem"));
            };
            let at = place(elem.location);
            let Some(column) = elem.name.as_deref() else {
                return Err(not_yet("an expression in the inference of ON CONFLICT").at_opt(at));
            };
            if !elem.collation.is_empty() || !elem.opclass.is_empty() {
                return Err(not_yet(
                    "a collation or an operator class in the inference of ON CONFLICT",
                )
                .at_opt(at));
            }
            let Some(index) = columns.iter().position(|c| c.name == column) else {
                return Err(Error::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!("column \"{column}\" does not exist"),
                )
                // `resolve_unique_index_expr` puts the column reference at the inference clause.
                .at_opt(place(infer.location)));
            };
            set.push(index);
        }
        set.sort_unstable();
        set.dedup();
        if let Some(predicate) = infer.whereClause.as_ref() {
            // The unique indexes that a write takes have no predicate, so the predicate of the inference does not change the match.
            let expr = self.with_kind(Kind::IndexPredicate, |a| a.transform(Some(predicate)))?;
            self.coerce_to_boolean(expr, "WHERE")?;
        }
        Ok(Inference::Columns(set))
    }

    /// `setTargetTable`: the OID, the name and the columns of the table that the statement writes. A table with a constraint or an object that the executor does not check yet gives `0A000`.
    pub(crate) fn target_table(
        &self,
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
        let rel = self.env.catalog().and_then(|c| c.relation(oid));
        let Some(rel) = rel else {
            return Err(not_yet(&format!("{what} a system catalog")));
        };
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
            RelKind::View => return Err(not_yet(&format!("{what} a view"))),
            RelKind::Partitioned => {}
            _ => {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("cannot change relation \"{name}\""),
                ));
            }
        }
        if let Some(catalog) = self.env.catalog()
            && catalog.rules().any(|r| r.relation == oid)
        {
            return Err(not_yet(&format!("{what} a table with rules")));
        }
        // A dropped column keeps its place with no name and the type 0, and each new row has a null for it.
        let columns = rel
            .columns
            .iter()
            .map(|c| Column {
                name: if c.dropped { String::new() } else { c.name.clone() },
                ty: c.ty,
                typmod: c.typmod,
                not_null: c.not_null,
            })
            .collect();
        Ok((oid, rel.name.clone(), columns))
    }

    /// True when the table has a trigger of the user that fires in the default mode and whose type has all the bits `bits`, such as `TRIGGER_ROW | TRIGGER_BEFORE | TRIGGER_INSERT`. With no bits, any such trigger counts.
    pub(crate) fn user_triggers(&self, table: u32, bits: i16) -> bool {
        let Some(catalog) = self.env.catalog() else { return false };
        catalog.triggers().any(|t| user_trigger(t, table) && t.kind & bits == bits)
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
    ) -> Option<Box<Query>> {
        let bits = rupg_catalog::TRIGGER_ROW | rupg_catalog::TRIGGER_BEFORE | trigger_event(kind);
        // The triggers of a partitioned table fire as their clones in the leaf partitions, which check the rows.
        if self.is_partitioned(table) || !self.user_triggers(table, bits) {
            return None;
        }
        let mut query = with_checks(name, columns, Query::default(), checks.to_vec(), &[]);
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
            .filter(|t| t.relation == table && t.fires())
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
                full: foreign.match_type == 'f',
                other,
                other_name: other_rel.name.clone(),
                columns: Vec::with_capacity(mine.len()),
                names: Vec::with_capacity(mine.len()),
                other_columns: Vec::with_capacity(mine.len()),
                types: Vec::with_capacity(mine.len()),
                referencing_types: Vec::with_capacity(mine.len()),
                constraint: constraint.oid,
                deferrable: trigger.deferrable,
                deferred: trigger.deferred,
                cloned: trigger.parent != 0,
            };
            for (&attnum, &other_attnum) in mine.iter().zip(theirs) {
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
            }
            if matches!(action, Action::Cascade | Action::SetNull | Action::SetDefault) {
                let names: Vec<&str> = check
                    .other_columns
                    .iter()
                    .map(|&at| other_rel.columns[at].name.as_str())
                    .collect();
                let schema = match catalog.schema(other_rel.namespace) {
                    Some(schema) => schema.name.clone(),
                    None => builtin::named_by_oid(Named::Namespace, other_rel.namespace)
                        .map_or_else(String::new, |n| n.name.to_string()),
                };
                let table = format!("{}.{}", quote_one(&schema), quote_one(&other_rel.name));
                let only = other_rel.kind != RelKind::Partitioned;
                check.query = action_query((action, kind), (&table, only), &names, &check);
            }
            list.push(check);
        }
        Ok(list)
    }

    /// The unique indexes of a table in the order of their OIDs. A unique index with an expression, a predicate or a deferrable constraint is not supported yet. The index of a partitioned table has no storage: the indexes of the partitions check the rows, and it is an arbiter of `ON CONFLICT`.
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
                    .filter(|info| info.table == table && info.unique)
                    .map(|info| (rel, info))
            })
            .collect();
        indexes.sort_by_key(|(rel, _)| rel.oid);
        let mut list = Vec::with_capacity(indexes.len());
        for (rel, info) in indexes {
            if info.exprs.is_some() || info.predicate.is_some() || !info.immediate {
                return Err(not_yet(
                    "a write to a table with a unique index on an expression, a partial unique index or a deferrable unique constraint",
                ));
            }
            let count = usize::try_from(info.key_count).unwrap_or_default();
            let mut key = Unique {
                oid: rel.oid,
                name: rel.name.clone(),
                columns: Vec::with_capacity(count),
                types: Vec::with_capacity(count),
                shown: String::new(),
                nulls_not_distinct: info.nulls_not_distinct,
            };
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

    /// `transformExpressionList` for a row of `VALUES` in an `INSERT`, which keeps `DEFAULT` at the top of an item.
    pub(crate) fn value_items(&mut self, list: &List, kind: Kind) -> Result<Vec<Item>> {
        let mut items = Vec::with_capacity(list.len());
        for node in list {
            if let Some(Node::SetToDefault(d)) = node {
                items.push(Item::Default(place(d.location)));
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
        let attnum = i16::try_from(index + 1).unwrap_or(i16::MAX);
        let sequence = self.env.catalog().and_then(|c| c.root_identity_sequence(table, attnum));
        if self.identity(table, index) != 0
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

    /// `INSERT ... VALUES` with more than one row: each row gets the types of the columns, and the rows are a `VALUES` relation that the row reads.
    fn insert_values(
        &mut self,
        select: &SelectStmt,
        table: u32,
        columns: &[Column],
        given: &[Given],
        explicit: bool,
        overriding: OverridingKind,
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
    pub(crate) fn checks(&self, table: u32) -> Result<Vec<(String, Expr)>> {
        let Some(catalog) = self.env.catalog() else { return Ok(Vec::new()) };
        let mut checks: Vec<(String, &str)> = catalog
            .constraints_of(table)
            .filter(|c| c.kind == ConKind::Check)
            .map(|c| (c.name.clone(), c.expr.as_deref().unwrap_or_default()))
            .collect();
        checks.sort_by(|a, b| a.0.cmp(&b.0));
        checks.into_iter().map(|(name, text)| Ok((name, crate::node::read(text)?))).collect()
    }
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
    /// The set of the columns of an inference, from 0, in order.
    Columns(Vec<usize>),
    /// The OID of the index of the constraint that `ON CONSTRAINT` names, or 0 for a constraint with no index.
    Constraint(u32),
}

/// `infer_arbiter_indexes`: the places in `unique` of the arbiter indexes. An inference takes each unique index whose key has the same set of columns.
fn arbiters(inference: Inference, unique: &[Unique]) -> Result<Vec<usize>> {
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
            if places.is_empty() {
                return Err(not_yet("an exclusion constraint in ON CONFLICT"));
            }
            return Ok(places);
        }
        Inference::Columns(set) => unique
            .iter()
            .enumerate()
            .filter(|(_, key)| {
                let mut key = key.columns.clone();
                key.sort_unstable();
                key.dedup();
                key == set
            })
            .map(|(at, _)| at)
            .collect(),
    };
    if places.is_empty() {
        return Err(Error::new(
            SqlState::INVALID_COLUMN_REFERENCE,
            "there is no unique or exclusion constraint matching the ON CONFLICT specification",
        ));
    }
    Ok(places)
}

/// True for a trigger of the table `table` that the user made and that fires in the default mode. The session checks the foreign keys itself, and a disabled trigger and a trigger for the replica mode do not fire.
fn user_trigger(t: &rupg_catalog::Trigger, table: u32) -> bool {
    t.relation == table && !t.internal && t.fires()
}

/// The bit of `tgtype` for the event of a write.
fn trigger_event(kind: WriteKind) -> i16 {
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

/// The query that reads the rows of `rows` as a subquery with the name of the table, and gives the columns and then the condition of each check constraint. The values of `tail` come after the columns in each row of `rows`, and after the conditions in each row of the query, as the carried values of `RETURNING` and the ID of the version of the old row of an `UPDATE`.
pub(crate) fn with_checks(
    name: &str,
    columns: &[Column],
    rows: Query,
    checks: Vec<(String, Expr)>,
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
    targets.extend(checks.into_iter().map(|(n, expr)| Target {
        name: n,
        expr,
        origin: None,
        junk: false,
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
