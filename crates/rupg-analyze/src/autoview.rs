//! `rewriteTargetView` of `rewriteHandler.c`: an `INSERT`, an `UPDATE` or a `DELETE` of a view with no trigger `INSTEAD OF` and no rule `DO INSTEAD` for the event writes the base relation of the view, when the view is automatically updatable.
//!
//! The base relation is the relation 0 of the write. The name of the view gives the columns of the view as expressions on the base relation, an `UPDATE` and a `DELETE` read only the rows of the base relation that the conditions of the view let through, and an item of `SET` or a column of an `INSERT` names the column of the base relation that the column of the view shows. A view on a view gives the columns and the conditions of both views on the base relation of the inner view.

use std::collections::{BTreeMap, BTreeSet};

use rupg_catalog::RelKind;
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{List, Node, RangeVar};

use crate::Analyzer;
use crate::expr::{CastForm, Expr, ExprKind};
use crate::from::{Column, FromItem, Relation};
use crate::insert::{Given, INSTEAD_ROW, WriteKind, not_yet, trigger_event};
use crate::merge::MergeCommand;
use crate::select::{Query, Target};

/// A column of a view that is not updatable, as `view_col_is_auto_updatable` finds it: the view, the column and the reason.
#[derive(Clone, Debug)]
pub(crate) struct Fixed {
    /// The name of the view.
    view: String,
    /// The name of the column.
    column: String,
    /// The number of views between the view of the write and this view, 0 for the view of the write. `rewriteTargetView` checks the outer view first.
    level: usize,
    /// The number of the column in its view.
    attnum: i16,
    /// The reason that the column is not updatable.
    detail: &'static str,
}

impl Fixed {
    /// The error of an `INSERT` or an `UPDATE` that gives the column a value.
    fn error(&self, kind: WriteKind) -> Error {
        let what = match kind {
            WriteKind::Insert => "insert into",
            WriteKind::Merge => "merge into",
            _ => "update",
        };
        Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("cannot {what} column \"{}\" of view \"{}\"", self.column, self.view),
        )
        .with_detail(self.detail)
    }
}

/// A column of an automatically updatable view.
#[derive(Clone, Debug)]
pub(crate) struct ViewColumn {
    /// The name of the column.
    name: String,
    /// The value of the column, which reads the base relation as the relation 0.
    expr: Expr,
    /// The number of the column of the base relation that the column shows, or the reason that the column is not updatable.
    base: std::result::Result<i16, Fixed>,
}

/// The target of a write of an automatically updatable view.
#[derive(Clone, Debug)]
pub(crate) struct AutoView {
    /// The OID of the view.
    oid: u32,
    /// The name of the view.
    name: String,
    /// The kind of the write.
    kind: WriteKind,
    /// The columns of the view.
    columns: Vec<ViewColumn>,
    /// The base relation, which is the relation 0 of the write.
    base: Relation,
    /// The conditions of the views with `security_barrier`, the inner view first. They come before the condition of the statement, as `securityQuals` do.
    barrier: Vec<Expr>,
    /// The conditions of the other views, the outer view first.
    quals: Vec<Expr>,
    /// For each view, the privileges of the relation that it reads, and the number of the column of that relation that each column of the view shows.
    levels: Vec<(Option<usize>, Vec<Option<i16>>)>,
    /// For each column of the base relation whose view column has a default of `ALTER VIEW ... SET DEFAULT`, the view and the number of the column in the view. The outer view comes first, as `rewriteTargetListIU` adds the defaults of each view before `rewriteTargetView`.
    defaults: BTreeMap<usize, (u32, i16)>,
    /// The defaults of the `INSERT` actions of a `MERGE`, as [`AutoView::defaults`] gives them for an `INSERT`. [`Analyzer::swap_auto_view_defaults`] puts them in place of the defaults of the `UPDATE` actions.
    insert_defaults: BTreeMap<usize, (u32, i16)>,
    /// For an `INSERT`, the number of the column of the view of each item of [`Given`], in the same order.
    given: Vec<i16>,
    /// The numbers of the columns of the view that an `INSERT` or an action `INSERT` of a `MERGE` gives values, for `insertedCols`.
    inserted: BTreeSet<i16>,
    /// The numbers of the columns of the view that an `UPDATE` or an action `UPDATE` of a `MERGE` gives values, for `updatedCols`.
    updated: BTreeSet<i16>,
    /// The checks of `WITH CHECK OPTION` of an `INSERT` or an `UPDATE`: the name of each view and its condition on the base relation, the inner view first.
    checks: Vec<(String, Expr)>,
}

/// The [`crate::ViewCheck`] of the checks `checks` on the base relation `rel`. The query reads the row that the statement wrote as the relation 0.
fn view_check(rel: &rupg_catalog::Relation, checks: Vec<(String, Expr)>) -> crate::ViewCheck {
    let mut relation = Relation::new(0, crate::insert::write_columns(rel), None, None, None);
    relation.name.clone_from(&rel.name);
    let (views, quals): (Vec<String>, Vec<Expr>) = checks.into_iter().unzip();
    let targets = quals
        .into_iter()
        .map(|expr| Target { name: String::new(), expr, origin: None, junk: false })
        .collect();
    let query = Query {
        relations: vec![relation],
        from: vec![FromItem::Relation(0)],
        targets,
        ..Query::default()
    };
    crate::ViewCheck { query: Box::new(query), views }
}

/// `view_col_is_auto_updatable`: the number of the column of the base relation `base` that a column of the query of a view shows, or the reason that the column is not updatable.
fn base_attnum(target: &Target, base: usize) -> std::result::Result<i16, &'static str> {
    if target.junk {
        return Err("Junk view columns are not updatable.");
    }
    match &target.expr.kind {
        ExprKind::Var(var) if var.relation == base && var.levels_up == 0 => match var.attnum {
            n if n < 0 => Err("View columns that refer to system columns are not updatable."),
            0 => Err("View columns that return whole-row references are not updatable."),
            n => Ok(n),
        },
        _ => Err("View columns that are not columns of their base relation are not updatable."),
    }
}

/// The expression with the relation `from` of its query as the relation 0, also in its subqueries.
fn to_relation_0(expr: &mut Expr, from: usize) {
    if from == 0 {
        return;
    }
    expr.each_mut(0, &mut |e, depth| {
        if let ExprKind::Var(var) = &mut e.kind
            && var.levels_up == depth
            && var.relation == from
        {
            var.relation = 0;
        }
        false
    });
}

/// The expression with the relation 0 of its query as the relation `to`, also in its subqueries.
fn from_relation_0(expr: &mut Expr, to: usize) {
    expr.each_mut(0, &mut |e, depth| {
        if let ExprKind::Var(var) = &mut e.kind
            && var.levels_up == depth
            && var.relation == 0
        {
            var.relation = to;
        }
        false
    });
}

/// True when the expression reads the whole row of the relation 0.
fn reads_whole_row(expr: &Expr) -> bool {
    expr.find(0, &mut |e, depth| match e.kind {
        ExprKind::Var(var) if var.levels_up == depth && var.relation == 0 && var.attnum == 0 => {
            Some(())
        }
        _ => None,
    })
    .is_some()
}

/// True when the expression reads a column of the relation 0 with a number in `read`.
fn reads_any(expr: &Expr, read: &BTreeSet<i16>) -> bool {
    expr.find(0, &mut |e, depth| match e.kind {
        ExprKind::Var(var)
            if var.levels_up == depth && var.relation == 0 && read.contains(&var.attnum) =>
        {
            Some(())
        }
        _ => None,
    })
    .is_some()
}

/// `error_view_not_updatable`: the error of a write of a view that is not automatically updatable, with the reason.
fn not_updatable(view: &str, kind: WriteKind, detail: &str) -> Error {
    let (message, hint) = match kind {
        WriteKind::Insert => (
            format!("cannot insert into view \"{view}\""),
            "To enable inserting into the view, provide an INSTEAD OF INSERT trigger or an unconditional ON INSERT DO INSTEAD rule.",
        ),
        WriteKind::Update | WriteKind::Merge => (
            format!("cannot update view \"{view}\""),
            "To enable updating the view, provide an INSTEAD OF UPDATE trigger or an unconditional ON UPDATE DO INSTEAD rule.",
        ),
        WriteKind::Delete => (
            format!("cannot delete from view \"{view}\""),
            "To enable deleting from the view, provide an INSTEAD OF DELETE trigger or an unconditional ON DELETE DO INSTEAD rule.",
        ),
    };
    Error::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, message)
        .with_detail(detail)
        .with_hint(hint)
}

/// `error_view_not_updatable` for a `MERGE`: the error of the action `kind` of a view that is not automatically updatable. `MERGE` does not use rules, so the hint names only the trigger.
fn merge_not_updatable(view: &str, kind: WriteKind, detail: &str) -> Error {
    let (message, hint) = match kind {
        WriteKind::Insert => (
            format!("cannot insert into view \"{view}\""),
            "To enable inserting into the view using MERGE, provide an INSTEAD OF INSERT trigger.",
        ),
        WriteKind::Delete => (
            format!("cannot delete from view \"{view}\""),
            "To enable deleting from the view using MERGE, provide an INSTEAD OF DELETE trigger.",
        ),
        _ => (
            format!("cannot update view \"{view}\""),
            "To enable updating the view using MERGE, provide an INSTEAD OF UPDATE trigger.",
        ),
    };
    Error::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, message)
        .with_detail(detail)
        .with_hint(hint)
}

/// The kind of the write of an action of a `MERGE`, or `None` for `DO NOTHING`.
fn action_kind(command: MergeCommand) -> Option<WriteKind> {
    match command {
        MergeCommand::Insert => Some(WriteKind::Insert),
        MergeCommand::Update => Some(WriteKind::Update),
        MergeCommand::Delete => Some(WriteKind::Delete),
        MergeCommand::Nothing => None,
    }
}

/// The first error of the columns of `fixed`, as `view_cols_are_auto_updatable` finds it: the outer view first, and in a view the first column.
fn first_fixed<'f>(fixed: impl Iterator<Item = &'f Fixed>, kind: WriteKind) -> Option<Error> {
    fixed.min_by_key(|f| (f.level, f.attnum)).map(|f| f.error(kind))
}

impl AutoView {
    /// The OID of the base relation.
    pub(crate) fn base_oid(&self) -> u32 {
        self.base.oid
    }
}

impl Analyzer<'_> {
    /// `view_query_is_auto_updatable`: the index of the base relation of the query of a view that the database can update with no rule, or the reason that it cannot. With `check_cols`, as for an `INSERT` and an `UPDATE`, the view must have an updatable column.
    pub(crate) fn auto_updatable(
        &self,
        query: &Query,
        check_cols: bool,
    ) -> std::result::Result<usize, &'static str> {
        const SINGLE: &str =
            "Views that do not select from a single table or view are not automatically updatable.";
        if !query.distinct.is_empty() {
            return Err("Views containing DISTINCT are not automatically updatable.");
        }
        if !query.group.is_empty() || !query.grouping_sets.is_empty() {
            return Err("Views containing GROUP BY are not automatically updatable.");
        }
        if query.having.is_some() {
            return Err("Views containing HAVING are not automatically updatable.");
        }
        if query.set_op.is_some() {
            return Err(
                "Views containing UNION, INTERSECT, or EXCEPT are not automatically updatable.",
            );
        }
        if !query.ctes.is_empty() {
            return Err("Views containing WITH are not automatically updatable.");
        }
        if query.limit.is_some() || query.offset.is_some() {
            return Err("Views containing LIMIT or OFFSET are not automatically updatable.");
        }
        if query.grouped {
            return Err("Views that return aggregate functions are not automatically updatable.");
        }
        if query.targets.iter().any(|t| t.expr.first_window().is_some()) {
            return Err("Views that return window functions are not automatically updatable.");
        }
        if query.has_target_srfs() {
            return Err(
                "Views that return set-returning functions are not automatically updatable.",
            );
        }
        let [FromItem::Relation(index)] = query.from.as_slice() else { return Err(SINGLE) };
        let relation = &query.relations[*index];
        let kind = self.env.catalog().and_then(|c| c.relation(relation.oid)).map(|r| r.kind);
        if relation.function.is_some()
            || relation.values.is_some()
            || relation.cte.is_some()
            || !matches!(kind, Some(RelKind::Table | RelKind::Partitioned | RelKind::View))
        {
            return Err(SINGLE);
        }
        if relation.tablesample.is_some() {
            return Err("Views containing TABLESAMPLE are not automatically updatable.");
        }
        if check_cols && !query.targets.iter().any(|t| base_attnum(t, *index).is_ok()) {
            return Err("Views that have no updatable columns are not automatically updatable.");
        }
        Ok(*index)
    }

    /// True when the relation has a rule for the event of the write. For a `MERGE`, a rule of any event other than `SELECT` counts, as `RewriteQuery` does not let a `MERGE` write such a relation.
    fn has_rules(&self, oid: u32, kind: WriteKind) -> bool {
        let code = match kind {
            WriteKind::Insert => b'3',
            WriteKind::Update => b'2',
            WriteKind::Delete => b'4',
            WriteKind::Merge => 0,
        };
        let event = |e: u8| if code == 0 { e != b'1' } else { e == code };
        self.env.catalog().is_some_and(|c| {
            c.rules().any(|r| r.relation == oid && event(r.event) && crate::rules::fires(r))
        })
    }

    /// `view_has_instead_trigger`: true when the view has a trigger `INSTEAD OF ... FOR EACH ROW` for the event of the write. For a `MERGE`, true when each action other than `DO NOTHING` has such a trigger, which is also true when all the actions are `DO NOTHING`.
    pub(crate) fn view_has_instead_trigger(&self, view: u32, kind: WriteKind) -> bool {
        if kind != WriteKind::Merge {
            return self.user_triggers(view, INSTEAD_ROW | trigger_event(kind));
        }
        self.merge_actions
            .iter()
            .filter_map(|&c| action_kind(c))
            .all(|kind| self.user_triggers(view, INSTEAD_ROW | trigger_event(kind)))
    }

    /// True when the view has a trigger `INSTEAD OF ... FOR EACH ROW` for the event of the write, or for a `MERGE`, for one of its actions.
    fn some_instead_trigger(&self, view: u32, kind: WriteKind) -> bool {
        if kind != WriteKind::Merge {
            return self.user_triggers(view, INSTEAD_ROW | trigger_event(kind));
        }
        self.merge_actions
            .iter()
            .filter_map(|&c| action_kind(c))
            .any(|kind| self.user_triggers(view, INSTEAD_ROW | trigger_event(kind)))
    }

    /// `error_view_not_updatable`: the error of a write of the view `view` with the OID `oid` that is not automatically updatable. For a `MERGE`, the error names the first action that has no trigger `INSTEAD OF`.
    fn view_not_updatable(&self, view: &str, oid: u32, kind: WriteKind, detail: &str) -> Error {
        if kind != WriteKind::Merge {
            return not_updatable(view, kind, detail);
        }
        let first = self
            .merge_actions
            .iter()
            .filter_map(|&c| action_kind(c))
            .find(|&kind| !self.user_triggers(oid, INSTEAD_ROW | trigger_event(kind)));
        merge_not_updatable(view, first.unwrap_or(WriteKind::Update), detail)
    }

    /// The part of `rewriteTargetView` for a `MERGE` of a view with triggers `INSTEAD OF` for some of its actions: such a view must have a trigger for each action or for none.
    ///
    /// # Errors
    ///
    /// `0A000` when the view has a trigger `INSTEAD OF` for an action of the `MERGE`.
    pub(crate) fn auto_view_merge_triggers(&self) -> Result<()> {
        let Some(auto) = &self.auto_view else { return Ok(()) };
        if !self.some_instead_trigger(auto.oid, WriteKind::Merge) {
            return Ok(());
        }
        Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("cannot merge into view \"{}\"", auto.name))
            .with_detail("MERGE is not supported for views with INSTEAD OF triggers for some actions but not all.")
            .with_hint("To enable merging into the view, either provide a full set of INSTEAD OF triggers or drop the existing INSTEAD OF triggers."))
    }

    /// The conditions of the views of an automatically updatable view that a `MERGE` writes, which limit the rows of the target table as `transform_MERGE_to_join` limits them: the conditions of the views of `security_barrier` first, then the conditions of the other views.
    pub(crate) fn auto_view_quals(&self) -> Option<Expr> {
        let auto = self.auto_view.as_ref()?;
        crate::rules::and_filter(None, auto.barrier.iter().chain(&auto.quals).cloned().collect())
    }

    /// `RelationHasCheckOption`: `Some(true)` for a view with `check_option=cascaded`, `Some(false)` for `check_option=local`, and `None` for a view with no check option.
    fn check_option(&self, view: u32) -> Option<bool> {
        let rel = self.env.catalog().and_then(|c| c.relation(view))?;
        let option = rel.options.iter().find_map(|o| o.strip_prefix("check_option="))?;
        Some(option == "cascaded")
    }

    /// True when the view has `security_barrier`.
    fn security_barrier(&self, view: u32) -> bool {
        self.env
            .catalog()
            .and_then(|c| c.relation(view))
            .is_some_and(|r| crate::perm::option_on(&r.options, "security_barrier"))
    }

    /// `rewriteTargetView`: the base relation of the view `oid` with the name `name` for a write of `kind`, with the columns and the conditions of the view and of the views that it reads. The privileges of the query of the view go after the privileges of the statement.
    ///
    /// # Errors
    ///
    /// `55000` when the view or a view that it reads is not automatically updatable.
    pub(crate) fn auto_view(&mut self, oid: u32, name: &str, kind: WriteKind) -> Result<AutoView> {
        // `insert_or_update`: an `INSERT`, an `UPDATE` or a `MERGE` with an action `INSERT` or `UPDATE`.
        let check_cols = match kind {
            WriteKind::Insert | WriteKind::Update => true,
            WriteKind::Delete => false,
            WriteKind::Merge => self
                .merge_actions
                .iter()
                .any(|c| matches!(c, MergeCommand::Insert | MergeCommand::Update)),
        };
        let (mut query, perms) =
            self.stored_query_perms(oid)?.ok_or_else(|| Error::internal("a view with no query"))?;
        self.add_view_perms(oid, &mut query, perms);
        let (mut view, mut view_oid) = (name.to_string(), oid);
        let mut columns: Vec<ViewColumn> = Vec::new();
        let (mut barrier, mut quals) = (Vec::new(), Vec::new());
        let (mut levels, mut views) = (Vec::new(), Vec::new());
        // `RelationHasCascadedCheckOption` of a view outside this view, which makes the check of each view under it cascaded.
        let (mut checks, mut cascading) = (Vec::new(), false);
        for level in 0.. {
            let index = self
                .auto_updatable(&query, check_cols)
                .map_err(|d| self.view_not_updatable(&view, view_oid, kind, d))?;
            let relation = query.relations[index].clone();
            let shown: Vec<&Target> = query.targets.iter().filter(|t| !t.junk).collect();
            let mut values = Vec::with_capacity(shown.len());
            let mut bases = Vec::with_capacity(shown.len());
            for (attnum, target) in (1i16..).zip(&shown) {
                let mut expr = target.expr.clone();
                to_relation_0(&mut expr, index);
                values.push(expr);
                bases.push(base_attnum(target, index).map_err(|detail| Fixed {
                    view: view.clone(),
                    column: target.name.clone(),
                    level,
                    attnum,
                    detail,
                }));
            }
            if level == 0 {
                columns = shown
                    .iter()
                    .zip(&values)
                    .zip(&bases)
                    .map(|((t, expr), base)| ViewColumn {
                        name: t.name.clone(),
                        expr: expr.clone(),
                        base: base.clone(),
                    })
                    .collect();
            } else {
                // `ReplaceVarsFromTargetList` of the next `rewriteTargetView`: the values of the columns of the inner view in place of its columns.
                let outer = columns.iter().map(|c| &c.expr).chain(&barrier).chain(&quals);
                let outer = outer.chain(checks.iter().map(|(_, qual)| qual));
                if outer.into_iter().any(reads_whole_row) {
                    return Err(not_yet("a whole-row reference to the view of an updatable view"));
                }
                for column in &mut columns {
                    crate::rules::replace_row(&mut column.expr, &values);
                    if let Ok(attnum) = column.base {
                        let at = usize::try_from(attnum - 1).ok().and_then(|i| bases.get(i));
                        column.base =
                            at.cloned().ok_or_else(|| Error::internal("a lost column"))?;
                    }
                }
                let checked = checks.iter_mut().map(|(_, qual)| qual);
                for qual in barrier.iter_mut().chain(&mut quals).chain(checked) {
                    crate::rules::replace_row(qual, &values);
                }
            }
            levels.push((relation.perm, bases.iter().map(|b| b.as_ref().ok().copied()).collect()));
            views.push(view_oid);
            let option = if check_cols { self.check_option(view_oid) } else { None };
            let cascaded = cascading || option == Some(true);
            if let Some(mut qual) = query.filter.take() {
                to_relation_0(&mut qual, index);
                // A check with no condition lets each row through, so only a check with a condition goes in the list.
                if cascaded || option.is_some() {
                    checks.insert(0, (view.clone(), qual.clone()));
                }
                if self.security_barrier(view_oid) {
                    barrier.insert(0, qual);
                } else {
                    quals.push(qual);
                }
            }
            cascading = cascaded;
            let inner = self.env.catalog().and_then(|c| c.relation(relation.oid));
            let Some(inner) = inner.filter(|r| r.kind == RelKind::View) else {
                if self.has_rules(relation.oid, kind) {
                    return Err(not_yet("a write of a view whose base relation has rules"));
                }
                let defaults = self.view_defaults(&views, &levels, kind);
                let insert_defaults = if kind == WriteKind::Merge {
                    self.view_defaults(&views, &levels, WriteKind::Insert)
                } else {
                    BTreeMap::new()
                };
                return Ok(AutoView {
                    oid,
                    name: name.to_string(),
                    kind,
                    columns,
                    base: relation,
                    barrier,
                    quals,
                    levels,
                    defaults,
                    insert_defaults,
                    given: Vec::new(),
                    inserted: BTreeSet::new(),
                    updated: BTreeSet::new(),
                    checks,
                });
            };
            if self.some_instead_trigger(relation.oid, kind) || self.has_rules(relation.oid, kind) {
                return Err(not_yet(
                    "a write of a view whose base view has INSTEAD OF triggers or rules",
                ));
            }
            view.clone_from(&inner.name);
            view_oid = relation.oid;
            query = *relation.subquery.ok_or_else(|| Error::internal("a view with no query"))?;
        }
        Err(Error::internal("an updatable view with no base relation"))
    }

    /// The defaults of the columns of the views `views` for the columns of the base relation, as [`AutoView::defaults`]. The maps of `levels` give the column of the relation under each view. In an `UPDATE`, `DEFAULT` gives the default of the column of the view of the statement, or a null, as `rewriteTargetListIU` puts the null in place of `DEFAULT` for a column with no default.
    fn view_defaults(
        &self,
        views: &[u32],
        levels: &[(Option<usize>, Vec<Option<i16>>)],
        kind: WriteKind,
    ) -> BTreeMap<usize, (u32, i16)> {
        let mut defaults = BTreeMap::new();
        let Some(catalog) = self.env.catalog() else { return defaults };
        let views = if kind == WriteKind::Insert { views } else { &views[..views.len().min(1)] };
        for (level, &view) in views.iter().enumerate() {
            for attnum in (1i16..).take(levels[level].1.len()) {
                if kind == WriteKind::Insert && catalog.default_of(view, attnum).is_none() {
                    continue;
                }
                let mut column = Some(attnum);
                for (_, map) in &levels[level..] {
                    column = column
                        .and_then(|n| map.get(usize::try_from(n - 1).ok()?).copied().flatten());
                }
                if let Some(index) = column.and_then(|n| usize::try_from(n - 1).ok()) {
                    defaults.entry(index).or_insert((view, attnum));
                }
            }
        }
        defaults
    }

    /// Puts the defaults of the `INSERT` actions of a `MERGE` in place of the defaults of its `UPDATE` actions, or back. `rewriteTargetListIU` gives the defaults of each action of a `MERGE` by the kind of the action.
    pub(crate) fn swap_auto_view_defaults(&mut self) {
        if let Some(auto) = self.auto_view.as_mut() {
            std::mem::swap(&mut auto.defaults, &mut auto.insert_defaults);
        }
    }

    /// The default of the column `index` of the base relation of an automatically updatable view from a view, or `None`: the view and the number of the column in the view.
    pub(crate) fn auto_view_default(&self, table: u32, index: usize) -> Option<(u32, i16)> {
        let auto = self.auto_view.as_ref().filter(|a| a.base.oid == table)?;
        auto.defaults.get(&index).copied()
    }

    /// The part of `setTargetTable` for an automatically updatable view: the base relation as the relation 0, and an item with the name of the view or its alias that gives the columns of the view. A view has no system columns.
    pub(crate) fn auto_view_item(&mut self, rv: &RangeVar) -> Result<Option<crate::from::Item>> {
        let Some(auto) = &self.auto_view else { return Ok(None) };
        let mut relation = auto.base.clone();
        relation.perm = self.target_perm;
        let alias = rv.alias.as_deref();
        let refname = alias.and_then(|a| a.aliasname.as_deref()).unwrap_or(&auto.name).to_string();
        let view = auto.oid;
        let columns: Vec<(String, Expr)> =
            auto.columns.iter().map(|c| (c.name.clone(), c.expr.clone())).collect();
        let mut item = self.add_relation(relation, refname, alias)?;
        let index = self.scope.relations.len() - 1;
        item.set_columns(columns.into_iter().map(|(n, mut e)| {
            from_relation_0(&mut e, index);
            (n, e)
        }));
        self.set_entry_oid(&item, view);
        Ok(Some(item))
    }

    /// The columns of the view as the item `old` or `new` of `RETURNING`, which reads the relation `index`.
    pub(crate) fn auto_view_returning(&mut self, item: &mut crate::from::Item, index: usize) {
        let Some(auto) = &self.auto_view else { return };
        let view = auto.oid;
        item.set_columns(auto.columns.iter().map(|c| {
            let mut expr = c.expr.clone();
            from_relation_0(&mut expr, index);
            (c.name.clone(), expr)
        }));
        self.set_entry_oid(item, view);
    }

    /// `markTargetListOrigins` for `RETURNING` of an automatically updatable view: a column of the view and the whole row of the view name the view and the number of the column in the view, as the parser sees the view before `rewriteTargetView`. Another value of the view has no origin.
    pub(crate) fn auto_view_origins(&self, targets: &mut [Target]) {
        let Some(auto) = &self.auto_view else { return };
        let shown = self.entry_relations(auto.oid);
        for target in targets {
            if let ExprKind::Row(_, CastForm::Implicit) = target.expr.kind
                && self.env.catalog().and_then(|c| c.relation(auto.oid)).map(|r| r.row_type)
                    == Some(target.expr.ty)
            {
                target.origin = Some((auto.oid, 0));
                continue;
            }
            let mut expr = target.expr.clone();
            let mut found = false;
            expr.each_mut(0, &mut |e, depth| {
                if let ExprKind::Var(var) = &mut e.kind
                    && var.levels_up == depth
                    && shown.contains(&var.relation)
                {
                    var.relation = 0;
                    found = true;
                }
                false
            });
            // A column of the view that reads no column of the base relation, such as a constant, has its name.
            let same = |c: &ViewColumn| c.expr.same(&expr);
            let named = auto.columns.iter().position(|c| c.name == target.name && same(c));
            let index =
                if found { named.or_else(|| auto.columns.iter().position(same)) } else { named };
            if !found && index.is_none() {
                continue;
            }
            target.origin = index.map(|i| (auto.oid, i16::try_from(i + 1).unwrap_or(i16::MAX)));
        }
    }

    /// The condition of an `UPDATE` or a `DELETE` of an automatically updatable view: the conditions of the views of `security_barrier`, the conditions of the other views, then the condition of the statement. `AddQual` puts the conditions of the views after the condition of the statement, but the planner of PostgreSQL runs a cheap condition first, and the executor runs the conditions in this order.
    pub(crate) fn auto_view_filter(&self, filter: Option<Expr>) -> Option<Expr> {
        let Some(auto) = &self.auto_view else { return filter };
        let parts = auto.barrier.iter().chain(&auto.quals).cloned().chain(filter);
        crate::rules::and_filter(None, parts.collect())
    }

    /// The items of `SET` of an `UPDATE` of an automatically updatable view, with the column of the base relation in place of each column of the view, or `None` for another write. The list comes with the error of an item whose column the view does not have, and with the error of a column of the view that is not updatable, which `rewriteTargetView` gives after the analysis.
    pub(crate) fn auto_view_set(
        &mut self,
        rv: &RangeVar,
        list: &List,
        columns: &[Column],
    ) -> Option<(List, Option<Error>, Option<Error>)> {
        let auto = self.auto_view.as_mut()?;
        let mut renamed = list.clone();
        let mut unknown = None;
        let mut fixed = Vec::new();
        for node in &mut renamed {
            let Some(Node::ResTarget(rt)) = node else { continue };
            let name = rt.name.as_deref().unwrap_or_default();
            let Some(index) = auto.columns.iter().position(|c| c.name == name) else {
                if unknown.is_none() {
                    unknown = Some(crate::modify::unknown_set_column(rv, &auto.name, rt));
                }
                continue;
            };
            auto.updated.insert(i16::try_from(index + 1).unwrap_or(i16::MAX));
            match &auto.columns[index].base {
                Ok(attnum) => {
                    let base = usize::try_from(attnum - 1).ok().and_then(|i| columns.get(i));
                    rt.name = base.map(|c| c.name.as_str().into());
                }
                Err(f) => fixed.push(f.clone()),
            }
        }
        let kind = auto.kind;
        Some((renamed, unknown, first_fixed(fixed.iter(), kind)))
    }

    /// `checkInsertTargets` for an `INSERT` of an automatically updatable view: the columns of the view that the statement names, or all of them, each as the column of the base relation that it shows. A column that is not updatable gets the number `width` and more, which [`Analyzer::auto_view_given`] checks.
    pub(crate) fn auto_view_targets(
        &mut self,
        cols: &List,
        width: usize,
    ) -> Result<Option<Vec<Given>>> {
        let Some(auto) = &self.auto_view else { return Ok(None) };
        let view_columns: Vec<Column> = auto
            .columns
            .iter()
            .map(|c| Column {
                name: c.name.clone(),
                ty: c.expr.ty,
                typmod: c.expr.typmod,
                not_null: false,
            })
            .collect();
        let name = auto.name.clone();
        let given = self.insert_targets(cols, &name, &view_columns)?;
        let Some(auto) = self.auto_view.as_mut() else { return Ok(None) };
        auto.given =
            given.iter().map(|(i, _, _)| i16::try_from(i + 1).unwrap_or(i16::MAX)).collect();
        let given = given
            .into_iter()
            .map(|(i, at, indirection)| match &auto.columns[i].base {
                Ok(attnum) => (usize::try_from(attnum - 1).unwrap_or_default(), at, indirection),
                Err(_) => (width + i, at, indirection),
            })
            .collect();
        Ok(Some(given))
    }

    /// The part of `rewriteTargetView` for the columns that an `INSERT` of an automatically updatable view gives, which are the first `count` columns of the list: each must be updatable.
    pub(crate) fn auto_view_given(&mut self, count: usize) -> Result<()> {
        let Some(auto) = self.auto_view.as_mut() else { return Ok(()) };
        let provided = auto.given.iter().take(count);
        auto.inserted.extend(provided.clone());
        let fixed = provided.filter_map(|&n| {
            let column = usize::try_from(n - 1).ok().and_then(|i| auto.columns.get(i))?;
            column.base.as_ref().err()
        });
        first_fixed(fixed, auto.kind).map_or(Ok(()), Err)
    }

    /// The end of the write of an automatically updatable view: the privileges of the view and of the relations under it, as `rewriteTargetView` gives the base relation the privileges that the statement needs on the view, with `adjust_view_column_set` for the columns.
    pub(crate) fn finish_auto_view(&mut self, query: &mut Query) {
        let Some(mut auto) = self.auto_view.take() else { return };
        if let Some(write) = query.write.as_mut()
            && !auto.checks.is_empty()
            && let Some(rel) = self.env.catalog().and_then(|c| c.relation(auto.base.oid))
        {
            write.view_check = Some(view_check(rel, std::mem::take(&mut auto.checks)));
        }
        let Some(view) = self.target_perm.and_then(|p| self.perms.get_mut(p)) else { return };
        // The statement read the columns of the base relation in the privileges of the view.
        let read = std::mem::take(&mut view.selected);
        view.selected = (1i16..)
            .zip(&auto.columns)
            .filter(|(_, c)| read.contains(&0) || reads_any(&c.expr, &read))
            .map(|(n, _)| n)
            .collect();
        view.inserted.clone_from(&auto.inserted);
        view.updated.clone_from(&auto.updated);
        let required = view.required;
        let (mut inserted, mut updated) = (auto.inserted, auto.updated);
        let down = |columns: &BTreeSet<i16>, map: &[Option<i16>]| -> BTreeSet<i16> {
            columns
                .iter()
                .filter_map(|&n| map.get(usize::try_from(n - 1).ok()?).copied().flatten())
                .collect()
        };
        for (perm, map) in &auto.levels {
            inserted = down(&inserted, map);
            updated = down(&updated, map);
            let Some(perm) = perm.and_then(|p| self.perms.get_mut(p)) else { continue };
            perm.required = required;
            perm.inserted.clone_from(&inserted);
            perm.updated.clone_from(&updated);
        }
    }
}
