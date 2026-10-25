//! `pg_get_viewdef` and the parts of `ruleutils.c` that show a query: `get_query_def`, the target list, `FROM`, the set operations, `ORDER BY`, the aggregate calls, the subqueries and the calls in the special syntax of the SQL standard.
//!
//! rupg keeps the text of the statement that made a view, not its query tree. So `pg_get_viewdef` analyzes the text again, with the `search_path` that the statement had, and shows the query as PostgreSQL shows the query tree that it keeps.

use std::collections::HashMap;

use rupg_analyze::{
    Column, Cte, FRAMEOPTION_END_OFFSET, FRAMEOPTION_END_UNBOUNDED_FOLLOWING,
    FRAMEOPTION_START_OFFSET, Frame, FromFunction, FromItem, GroupingSet, Join, JoinKind, JoinRow,
    NullTreatment, Query, Relation, SetKind, SetOp, SetTree, TableSample,
};
use rupg_sql::nodes::{
    FRAMEOPTION_BETWEEN, FRAMEOPTION_END_CURRENT_ROW, FRAMEOPTION_END_OFFSET_PRECEDING,
    FRAMEOPTION_EXCLUDE_CURRENT_ROW, FRAMEOPTION_EXCLUDE_GROUP, FRAMEOPTION_EXCLUDE_TIES,
    FRAMEOPTION_NONDEFAULT, FRAMEOPTION_RANGE, FRAMEOPTION_ROWS, FRAMEOPTION_START_CURRENT_ROW,
    FRAMEOPTION_START_OFFSET_PRECEDING, FRAMEOPTION_START_UNBOUNDED_PRECEDING,
};
use rupg_types::qualified_name_list;

use super::{
    Aggref, Deparser, Error, Expr, ExprKind, Func, FuncForm, INDENT_STD, INDENT_VAR, JoinNames,
    JsonCtor, Namespace, Result, SortGroup, SubLink, SubLinkKind, Value, WindowClause, WindowFunc,
    builtin, column_names, json_format, looks_like_function, oid, pretty_arg,
    qualified_relation_name, quote_identifier, reg, relation_name, sort_operators,
};
use crate::{Call, Session, bad_value, not_yet, type_error};
use rupg_catalog::{
    TRIGGER_BEFORE, TRIGGER_DELETE, TRIGGER_INSERT, TRIGGER_INSTEAD, TRIGGER_ROW, TRIGGER_TRUNCATE,
    TRIGGER_UPDATE,
};

/// `PRETTYINDENT_JOIN`: the indent of a `JOIN` keyword after the start of its line.
const INDENT_JOIN: i32 = 4;
/// `F_UNNEST_ANYARRAY`: the function `unnest(anyarray)`.
const UNNEST: u32 = 2331;

impl Deparser<'_> {
    /// `get_query_def`: the query with the names of its relations, in the queries outside it. `result` has the names of the columns of the result when the caller knows them, and `indent` is the indent of the query.
    pub(super) fn query_def(
        &mut self,
        query: &Query,
        result: Option<&[String]>,
        names_visible: bool,
        indent: i32,
    ) -> Result<()> {
        let space = self.namespace(query);
        let joins: usize = query.from.iter().map(join_count).sum();
        // The body of a function in SQL has the namespace of the function outside the query.
        let prefix = !self.namespaces.is_empty()
            || self.function.is_some()
            || query.relations.len() + joins != 1;
        let outputs = query
            .targets
            .iter()
            .filter(|t| !t.junk)
            .enumerate()
            .map(|(i, t)| {
                let name = result.and_then(|r| r.get(i)).unwrap_or(&t.name).clone();
                let var = match &t.expr.kind {
                    ExprKind::Var(var) => Some(*var),
                    _ => None,
                };
                (name, var)
            })
            .collect();
        let saved_indent = std::mem::replace(&mut self.indent, indent);
        let saved_prefix = std::mem::replace(&mut self.prefix, prefix);
        let saved_visible = std::mem::replace(&mut self.names_visible, names_visible);
        let saved_group = std::mem::replace(&mut self.in_group_by, false);
        let saved_outputs = std::mem::replace(&mut self.outputs, outputs);
        let saved_windows = std::mem::replace(&mut self.windows, query.windows.clone());
        let targets = query.targets.iter().map(|t| t.expr.clone()).collect();
        let saved_targets = std::mem::replace(&mut self.window_targets, targets);
        self.namespaces.push(space);
        let done = self.select_query(query, result);
        self.namespaces.pop();
        self.windows = saved_windows;
        self.window_targets = saved_targets;
        self.indent = saved_indent;
        self.prefix = saved_prefix;
        self.names_visible = saved_visible;
        self.in_group_by = saved_group;
        self.outputs = saved_outputs;
        done
    }

    /// `set_rtable_names`, `set_using_names`, `set_relation_column_names` and `set_join_column_names`: a name for each relation and each join with an alias that no other relation of the query or of the queries outside it has, and the names of the columns of each relation and each join. The names come in the order of the range table, where a join comes after the relations in it.
    fn namespace(&self, query: &Query) -> Namespace {
        let mut used: HashMap<String, u32> = HashMap::new();
        for space in &self.namespaces {
            let joins = space.joins.iter().filter_map(|j| j.name.as_ref());
            for name in space.names.iter().filter(|n| !n.is_empty()).chain(joins) {
                used.insert(name.clone(), 0);
            }
        }
        self.namespace_in(query, &mut used, false, &[])
    }

    /// The name of a relation before `set_rtable_names` makes it unique: the alias, or else the name of the table.
    pub(super) fn base_name(&self, rel: &Relation) -> String {
        match reg::class_name(rel.oid, self.session) {
            Some((name, _)) if rel.alias.is_none() && rel.oid != 0 => name.to_string(),
            _ => rel.name.clone(),
        }
    }

    /// [`Deparser::namespace`] with the names that are in use. For `EXPLAIN`, as `select_rtable_names_for_explain`, a join has no name, because no node of the plan reads a join, and a column of `USING` shows as its value. The relations in `hidden` take no unique name.
    pub(super) fn namespace_in(
        &self,
        query: &Query,
        used: &mut HashMap<String, u32>,
        explain: bool,
        hidden: &[usize],
    ) -> Namespace {
        let mut joins = Vec::new();
        let mut order = Vec::new();
        let tops: Vec<Slot> =
            query.from.iter().map(|item| walk_from(item, &mut joins, &mut order)).collect();
        let mut seen = vec![false; query.relations.len()];
        for slot in &order {
            if let Slot::Relation(i) = slot
                && let Some(s) = seen.get_mut(*i)
            {
                *s = true;
            }
        }
        order.extend(seen.iter().enumerate().filter(|(_, s)| !**s).map(|(i, _)| Slot::Relation(i)));
        let mut space =
            Namespace { names: vec![String::new(); query.relations.len()], ..Default::default() };
        let mut join_names: Vec<Option<String>> = vec![None; joins.len()];
        // `select_rtable_names_for_explain` names the relations in the order of the range table, where the relations of a subquery that `pull_up_subqueries` merged into the query come last.
        let mut naming = order.clone();
        if explain {
            naming.sort_by_key(|slot| match *slot {
                Slot::Relation(i) => (!seen[i], i),
                Slot::Join(_) => (true, usize::MAX),
            });
        }
        for slot in &naming {
            match *slot {
                Slot::Relation(i) => {
                    let Some(rel) = query.relations.get(i) else { continue };
                    // A relation that the plan does not scan has no name in the range table.
                    if hidden.contains(&i) {
                        space.names[i] = self.base_name(rel);
                        continue;
                    }
                    space.names[i] = unique_name(self.base_name(rel), used);
                }
                Slot::Join(k) => {
                    if let Some(alias) = &joins[k].join.alias
                        && !explain
                    {
                        join_names[k] = Some(unique_name(alias.clone(), used));
                    }
                }
            }
        }
        let mut state = ColumnNames {
            relations: query.relations.iter().map(|r| vec![None; r.columns.len()]).collect(),
            relation_parents: vec![Vec::new(); query.relations.len()],
            joins: joins.iter().map(|j| vec![None; j.join.columns.len()]).collect(),
            join_parents: vec![Vec::new(); joins.len()],
            join_using: vec![Vec::new(); joins.len()],
            using: Vec::new(),
            unique_using: joins
                .iter()
                .any(|j| j.join.alias.is_none() && !j.join.using.is_empty() && !j.join.plain_using),
            live: query.relations.iter().map(Relation::live_columns).collect(),
        };
        for slot in tops {
            if let Slot::Join(k) = slot {
                state.using_names(&joins, k, &[]);
            }
        }
        // `set_relation_column_names` reads the names of the columns of a table or a view from the catalog, so a column that `ALTER ... RENAME` renamed after the analysis of the query shows its new name. A column that `ALTER TABLE ... ADD COLUMN` added after the analysis has a name too, which the other names of the relation do not have. A dropped column has no name.
        let catalog = self.session.catalog();
        let real_names: Vec<Vec<String>> = query
            .relations
            .iter()
            .map(|rel| {
                let now = catalog
                    .and_then(|c| c.relation(rel.oid))
                    .filter(|now| rel.oid != 0 && now.columns.len() >= rel.columns.len());
                match now {
                    Some(now) => now
                        .columns
                        .iter()
                        .map(|c| if c.dropped { String::new() } else { c.name.clone() })
                        .collect(),
                    None => rel.columns.iter().map(|c| c.name.clone()).collect(),
                }
            })
            .collect();
        for (names, real) in state.relations.iter_mut().zip(&real_names) {
            names.resize(real.len().max(names.len()), None);
        }
        let mut print_join = vec![false; joins.len()];
        for slot in &order {
            match *slot {
                Slot::Relation(i) => {
                    let Some(rel) = query.relations.get(i) else { continue };
                    let given = rel.alias_names();
                    for (c, column) in real_names[i].iter().enumerate() {
                        if state.relations[i][c].is_none() && column.is_empty() {
                            state.relations[i][c] = Some(String::new());
                        } else if state.relations[i][c].is_none() {
                            let wanted = given.get(c).copied().flatten().unwrap_or(column);
                            let name = state.unique(wanted, Slot::Relation(i));
                            state.relations[i][c] = Some(name);
                        }
                    }
                }
                Slot::Join(k) => print_join[k] = state.join_columns(&joins, k),
            }
        }
        for (i, rel) in query.relations.iter().enumerate() {
            let given = rel.alias_names();
            let names: Vec<String> =
                state.relations[i].iter().map(|n| n.clone().unwrap_or_default()).collect();
            // The real name of a column of a table or a view is the name in the catalog. For another relation, it is the name that the alias gives.
            let changed =
                real_names[i].iter().zip(&names).enumerate().any(|(c, (column, name))| {
                    let real = if rel.oid != 0 {
                        column.as_str()
                    } else {
                        given.get(c).copied().flatten().unwrap_or(column)
                    };
                    name != real
                });
            // `JSON_TABLE` does not show the names of its columns after its alias.
            let print = if rel.json_table().is_some() {
                false
            } else if rel.oid != 0 {
                changed
            } else {
                rel.function.is_some()
                    || rel.alias.as_ref().is_some_and(|a| !a.is_empty())
                    || changed
            };
            space.columns.push(names);
            space.print_aliases.push(print);
        }
        for (k, info) in joins.iter().enumerate() {
            space.joins.push(JoinNames {
                name: join_names[k].clone(),
                relations: info.relations.clone(),
                columns: state.joins[k].iter().map(|n| n.clone().unwrap_or_default()).collect(),
                using: std::mem::take(&mut state.join_using[k]),
                values: info.join.columns.iter().map(|(_, e)| e.clone()).collect(),
                print_aliases: print_join[k],
            });
        }
        // A column of `USING` shows the name that the deparse gives it in `USING`, which can differ from the name of the query when two joins have a column of the same name, as `get_variable` reads it from `colinfo`.
        if !explain {
            for (info, names) in joins.iter().zip(&space.joins) {
                merged_columns(info.join, &names.using, &mut space.merged);
            }
        }
        space.relations.clone_from(&query.relations);
        space
    }

    /// `get_variable` for a column of a join with an alias: the name of the join and the name of the column, when the expression is the value of a column of a join with an alias outside the part of `FROM` that the deparse shows now. The alias hides the relations in the join, so the query reads such a column through the join.
    pub(super) fn join_column(&self, e: &Expr) -> Option<(String, String)> {
        for (levels, space) in self.namespaces.iter().rev().enumerate() {
            for (k, join) in space.joins.iter().enumerate() {
                let Some(name) = &join.name else { continue };
                if space.inside.contains(&k) {
                    continue;
                }
                for (value, column) in join.values.iter().zip(&join.columns) {
                    let found = match (&value.kind, &e.kind) {
                        (ExprKind::Var(v), ExprKind::Var(w)) => {
                            v.relation == w.relation
                                && v.attnum == w.attnum
                                && v.levels_up + levels == w.levels_up
                        }
                        (ExprKind::Var(_), _) | (_, ExprKind::Var(_)) => false,
                        _ if std::mem::discriminant(&value.kind)
                            != std::mem::discriminant(&e.kind) =>
                        {
                            false
                        }
                        _ => {
                            let mut value = value.clone();
                            value.raise(levels);
                            value.same(e)
                        }
                    };
                    if found {
                        return Some((name.clone(), column.clone()));
                    }
                }
            }
        }
        None
    }

    /// The name of the join with an alias that gives the fields of a whole row, or the alias of the analyzer when no join of the queries gives them.
    pub(super) fn join_row_name(&self, row: &JoinRow) -> String {
        for (levels, space) in self.namespaces.iter().rev().enumerate() {
            for join in &space.joins {
                let Some(name) = &join.name else { continue };
                let same = join.values.len() == row.fields.len()
                    && join.values.iter().zip(&row.fields).all(|(value, field)| {
                        let mut value = value.clone();
                        value.raise(levels);
                        value.same(field)
                    });
                if same {
                    return name.clone();
                }
            }
        }
        row.alias.clone().unwrap_or_default()
    }

    /// The name of a column of `USING` that is not a column of one side, or of a column of a join with an alias, when the expression is its value in this query or in a query outside it.
    pub(super) fn merged_name(&self, e: &Expr) -> Option<String> {
        if let Some((_, column)) = self.join_column(e) {
            return Some(column);
        }
        for (levels, space) in self.namespaces.iter().rev().enumerate() {
            for (value, name) in &space.merged {
                let found = if levels == 0 {
                    value.same(e)
                } else {
                    let mut value = value.clone();
                    value.raise(levels);
                    value.same(e)
                };
                if found {
                    return Some(name.clone());
                }
            }
        }
        None
    }

    /// `get_select_query_def`: the query, then `ORDER BY`, `OFFSET` and `LIMIT`.
    fn select_query(&mut self, query: &Query, result: Option<&[String]>) -> Result<()> {
        self.with_clause(query)?;
        let force = match &query.set_op {
            Some(op) => {
                self.set_op(op, query, result)?;
                true
            }
            None => {
                self.basic_select(query, result)?;
                false
            }
        };
        if !query.sort.is_empty() {
            self.keyword(" ORDER BY ", -INDENT_STD, INDENT_STD, 1);
            let exprs: Vec<&Expr> = query.targets.iter().map(|t| &t.expr).collect();
            self.order_by(&query.sort, &exprs, force)?;
        }
        if let Some(offset) = &query.offset {
            self.keyword(" OFFSET ", -INDENT_STD, INDENT_STD, 0);
            self.expr(offset, false)?;
        }
        if let Some(limit) = &query.limit {
            if query.with_ties {
                self.keyword(" FETCH FIRST ", -INDENT_STD, INDENT_STD, 0);
                self.buf.push('(');
                self.expr(limit, false)?;
                self.buf.push_str(") ROWS WITH TIES");
            } else {
                self.keyword(" LIMIT ", -INDENT_STD, INDENT_STD, 0);
                match &limit.kind {
                    ExprKind::Const(value) if value.is_null() => self.buf.push_str("ALL"),
                    _ => self.expr(limit, false)?,
                }
            }
        }
        Ok(())
    }

    /// `get_with_clause`: the queries of `WITH`, each in parentheses on its own lines.
    fn with_clause(&mut self, query: &Query) -> Result<()> {
        if query.ctes.is_empty() {
            return Ok(());
        }
        self.indent += INDENT_STD;
        if !self.flat {
            self.buf.push(' ');
        }
        let mut sep = if query.recursive { "WITH RECURSIVE " } else { "WITH " };
        for cte in &query.ctes {
            self.buf.push_str(sep);
            self.buf.push_str(&quote_identifier(&cte.name));
            if !cte.aliases.is_empty() {
                let names: Vec<String> = cte.aliases.iter().map(|n| quote_identifier(n)).collect();
                self.buf.push_str(&format!("({})", names.join(", ")));
            }
            self.buf.push_str(" AS ");
            match cte.materialized {
                Some(true) => self.buf.push_str("MATERIALIZED "),
                Some(false) => self.buf.push_str("NOT MATERIALIZED "),
                None => {}
            }
            self.buf.push('(');
            self.keyword("", 0, 0, 0);
            self.query_def(&cte.query, None, true, self.indent)?;
            self.keyword("", 0, 0, 0);
            self.buf.push(')');
            self.search_cycle(cte)?;
            sep = ", ";
        }
        self.indent -= INDENT_STD;
        self.keyword("", 0, 0, 0);
        Ok(())
    }

    /// The `SEARCH` and `CYCLE` clauses of a query of `WITH`. `CYCLE` shows `TO` and `DEFAULT` unless the marks are `true` and `false`.
    fn search_cycle(&mut self, cte: &Cte) -> Result<()> {
        let names = |list: &[String]| -> String {
            list.iter().map(|n| quote_identifier(n)).collect::<Vec<_>>().join(", ")
        };
        if let Some(s) = &cte.search {
            let order = if s.breadth { "BREADTH" } else { "DEPTH" };
            let columns = names(&s.columns);
            let seq = quote_identifier(&s.seq);
            self.buf.push_str(&format!(" SEARCH {order} FIRST BY {columns} SET {seq}"));
        }
        if let Some(c) = &cte.cycle {
            let columns = names(&c.columns);
            let mark = quote_identifier(&c.mark);
            self.buf.push_str(&format!(" CYCLE {columns} SET {mark}"));
            let plain = matches!(c.value.kind, ExprKind::Const(Value::Bool(true)))
                && matches!(c.default.kind, ExprKind::Const(Value::Bool(false)))
                && c.ty == oid::BOOL;
            if !plain {
                self.buf.push_str(" TO ");
                self.expr(&c.value, false)?;
                self.buf.push_str(" DEFAULT ");
                self.expr(&c.default, false)?;
            }
            self.buf.push_str(&format!(" USING {}", quote_identifier(&c.path)));
        }
        Ok(())
    }

    /// `get_basic_select_query`: a `SELECT` with no set operation, or a plain `VALUES` list.
    fn basic_select(&mut self, query: &Query, result: Option<&[String]>) -> Result<()> {
        self.indent += INDENT_STD;
        if !self.flat {
            self.buf.push(' ');
        }
        if let Some(rows) = simple_values(query, result) {
            return self.values(rows);
        }
        self.buf.push_str("SELECT");
        if !query.distinct.is_empty() {
            if query.distinct_on {
                self.buf.push_str(" DISTINCT ON (");
                for (i, item) in query.distinct.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(", ");
                    }
                    let e = target_expr(query, item)?;
                    self.sort_group(e, item.target + 1, false)?;
                }
                self.buf.push(')');
            } else {
                self.buf.push_str(" DISTINCT");
            }
        }
        self.target_list(query, result)?;
        self.clause_from(query)?;
        if let Some(filter) = &query.filter {
            self.keyword(" WHERE ", -INDENT_STD, INDENT_STD, 1);
            self.expr(filter, false)?;
        }
        if !query.group.is_empty() || !query.grouping_sets.is_empty() {
            self.keyword(" GROUP BY ", -INDENT_STD, INDENT_STD, 1);
            if query.group_distinct {
                self.buf.push_str("DISTINCT ");
            }
            let saved = std::mem::replace(&mut self.in_group_by, true);
            if query.grouping_sets.is_empty() {
                for (i, item) in query.group.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(", ");
                    }
                    let e = target_expr(query, item)?;
                    self.sort_group(e, item.target + 1, false)?;
                }
            } else {
                for (i, set) in query.grouping_sets.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(", ");
                    }
                    self.grouping_set(query, set, true)?;
                }
            }
            self.in_group_by = saved;
        }
        if let Some(having) = &query.having {
            self.keyword(" HAVING ", -INDENT_STD, INDENT_STD, 0);
            self.expr(having, false)?;
        }
        // get_rule_windowclause: the windows with a name.
        let mut first = true;
        for window in &query.windows {
            let Some(name) = &window.name else { continue };
            if first {
                self.keyword(" WINDOW ", -INDENT_STD, INDENT_STD, 1);
            } else {
                self.buf.push_str(", ");
            }
            first = false;
            self.buf.push_str(&format!("{} AS ", quote_identifier(name)));
            self.window_spec(window)?;
        }
        Ok(())
    }

    /// `get_rule_windowspec`: the window in parentheses. The items of `PARTITION BY` show only for a window that copies no window, and the items of `ORDER BY` show only when they are not a copy.
    fn window_spec(&mut self, window: &WindowClause) -> Result<()> {
        let targets = std::mem::take(&mut self.window_targets);
        let exprs: Vec<&Expr> = targets.iter().collect();
        let done = self.window_parts(window, &exprs);
        self.window_targets = targets;
        done
    }

    /// The parts of [`Deparser::window_spec`], with the expressions of the targets.
    fn window_parts(&mut self, window: &WindowClause, exprs: &[&Expr]) -> Result<()> {
        self.buf.push('(');
        let mut space = false;
        if let Some(refname) = &window.refname {
            self.buf.push_str(&quote_identifier(refname));
            space = true;
        }
        if !window.partition.is_empty() && window.refname.is_none() {
            if space {
                self.buf.push(' ');
            }
            self.buf.push_str("PARTITION BY ");
            for (i, item) in window.partition.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                let e = exprs
                    .get(item.target)
                    .ok_or_else(|| Error::internal("an item of PARTITION BY with no expression"))?;
                self.sort_group(e, item.target + 1, false)?;
            }
            space = true;
        }
        if !window.order.is_empty() && !window.copied_order {
            if space {
                self.buf.push(' ');
            }
            self.buf.push_str("ORDER BY ");
            self.order_by(&window.order, exprs, false)?;
            space = true;
        }
        if window.frame.options & FRAMEOPTION_NONDEFAULT != 0 {
            if space {
                self.buf.push(' ');
            }
            self.frame(&window.frame)?;
        }
        self.buf.push(')');
        Ok(())
    }

    /// `get_window_frame_options`: the mode of the frame, its bounds and its exclusion.
    pub(super) fn frame(&mut self, frame: &Frame) -> Result<()> {
        let options = frame.options;
        self.buf.push_str(if options & FRAMEOPTION_RANGE != 0 {
            "RANGE "
        } else if options & FRAMEOPTION_ROWS != 0 {
            "ROWS "
        } else {
            "GROUPS "
        });
        if options & FRAMEOPTION_BETWEEN != 0 {
            self.buf.push_str("BETWEEN ");
        }
        if options & FRAMEOPTION_START_UNBOUNDED_PRECEDING != 0 {
            self.buf.push_str("UNBOUNDED PRECEDING ");
        } else if options & FRAMEOPTION_START_CURRENT_ROW != 0 {
            self.buf.push_str("CURRENT ROW ");
        } else if options & FRAMEOPTION_START_OFFSET != 0 {
            let offset = frame
                .start
                .as_ref()
                .ok_or_else(|| Error::internal("a frame start with no offset"))?;
            self.expr(offset, false)?;
            self.buf.push_str(if options & FRAMEOPTION_START_OFFSET_PRECEDING != 0 {
                " PRECEDING "
            } else {
                " FOLLOWING "
            });
        }
        if options & FRAMEOPTION_BETWEEN != 0 {
            self.buf.push_str("AND ");
            if options & FRAMEOPTION_END_UNBOUNDED_FOLLOWING != 0 {
                self.buf.push_str("UNBOUNDED FOLLOWING ");
            } else if options & FRAMEOPTION_END_CURRENT_ROW != 0 {
                self.buf.push_str("CURRENT ROW ");
            } else if options & FRAMEOPTION_END_OFFSET != 0 {
                let offset = frame
                    .end
                    .as_ref()
                    .ok_or_else(|| Error::internal("a frame end with no offset"))?;
                self.expr(offset, false)?;
                self.buf.push_str(if options & FRAMEOPTION_END_OFFSET_PRECEDING != 0 {
                    " PRECEDING "
                } else {
                    " FOLLOWING "
                });
            }
        }
        if options & FRAMEOPTION_EXCLUDE_CURRENT_ROW != 0 {
            self.buf.push_str("EXCLUDE CURRENT ROW ");
        } else if options & FRAMEOPTION_EXCLUDE_GROUP != 0 {
            self.buf.push_str("EXCLUDE GROUP ");
        } else if options & FRAMEOPTION_EXCLUDE_TIES != 0 {
            self.buf.push_str("EXCLUDE TIES ");
        }
        // Each part ends with a space, and the last one does not keep it.
        self.buf.pop();
        Ok(())
    }

    /// `get_windowfunc_expr`: a call of a window function, then `OVER` and the name of its window or the window in parentheses.
    pub(super) fn window_func(&mut self, w: &WindowFunc) -> Result<()> {
        self.window_func_as(w, None)
    }

    /// `get_windowfunc_expr_helper`: a call of a window function. `json` has the name, the options and the kind of a SQL/JSON aggregate, where `JSON_OBJECTAGG` puts ` : ` between its two arguments.
    pub(super) fn window_func_as(
        &mut self,
        w: &WindowFunc,
        json: Option<(&str, &str, bool)>,
    ) -> Result<()> {
        match json {
            Some((name, _, _)) => self.buf.push_str(name),
            None => {
                let proc = builtin::proc_by_oid(w.oid).ok_or_else(|| {
                    Error::internal(format!("cache lookup failed for function {}", w.oid))
                })?;
                self.buf.push_str(&quote_identifier(proc.name));
            }
        }
        self.buf.push('(');
        if w.star {
            self.buf.push('*');
        } else if let Some((_, _, true)) = json {
            let [key, value, ..] = w.args.as_slice() else {
                return Err(Error::internal("JSON_OBJECTAGG with less than two arguments"));
            };
            self.expr(key, false)?;
            self.buf.push_str(" : ");
            self.expr(value, false)?;
        } else {
            for (i, arg) in w.args.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                self.expr(arg, true)?;
            }
        }
        if let Some((_, options, _)) = json {
            self.buf.push_str(options);
        }
        if let Some(filter) = &w.filter {
            self.buf.push_str(") FILTER (WHERE ");
            self.expr(filter, false)?;
        }
        self.buf.push_str(") ");
        if w.nulls == NullTreatment::Ignore {
            self.buf.push_str("IGNORE NULLS ");
        }
        self.buf.push_str("OVER ");
        if let Some(Some(name)) = w.winref.checked_sub(1).and_then(|i| self.window_names.get(i)) {
            self.buf.push_str(&quote_identifier(name));
            return Ok(());
        }
        let window = w.winref.checked_sub(1).and_then(|i| self.windows.get(i)).cloned();
        let Some(window) = window else {
            return Err(Error::internal(format!(
                "could not find window clause for winref {}",
                w.winref
            )));
        };
        match &window.name {
            Some(name) => self.buf.push_str(&quote_identifier(name)),
            None => self.window_spec(&window)?,
        }
        Ok(())
    }

    /// `get_values_def`: the rows of a `VALUES` list.
    fn values(&mut self, rows: &[Vec<Expr>]) -> Result<()> {
        self.buf.push_str("VALUES ");
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            self.buf.push('(');
            for (j, value) in row.iter().enumerate() {
                if j > 0 {
                    self.buf.push(',');
                }
                self.toplevel(value, false)?;
            }
            self.buf.push(')');
        }
        Ok(())
    }

    /// `get_target_list`: the columns of the result, each with `AS name` when its text does not give the name. A column after the first starts a new line when the line gets longer than the wrap column.
    fn target_list(&mut self, query: &Query, result: Option<&[String]>) -> Result<()> {
        let mut multiline = false;
        for (i, target) in query.targets.iter().filter(|t| !t.junk).enumerate() {
            self.buf.push_str(if i == 0 { " " } else { ", " });
            let outer = std::mem::take(&mut self.buf);
            let merged = self.merged_name(&target.expr);
            let attname = match &target.expr.kind {
                _ if merged.is_some() => {
                    self.expr(&target.expr, true)?;
                    merged
                }
                ExprKind::Var(var) if var.attnum == 0 => {
                    self.whole_row(var, &target.expr, true)?;
                    if self.names_visible { None } else { Some("?column?".to_string()) }
                }
                // `get_variable` shows the type of a whole row in the list of the query.
                ExprKind::JoinRow(row) if row.alias.is_some() => {
                    self.expr(&target.expr, true)?;
                    self.buf.push_str("::record");
                    if self.names_visible { None } else { Some("?column?".to_string()) }
                }
                ExprKind::Var(var) => Some(self.variable(var, false)?),
                _ => {
                    self.expr(&target.expr, true)?;
                    if self.names_visible { None } else { Some("?column?".to_string()) }
                }
            };
            let colname = result.and_then(|r| r.get(i)).unwrap_or(&target.name);
            if attname.as_deref() != Some(colname.as_str()) {
                self.buf.push_str(&format!(" AS {}", quote_identifier(colname)));
            }
            let item = std::mem::replace(&mut self.buf, outer);
            if self.wrap >= 0 {
                let leading = item.starts_with('\n');
                if leading {
                    self.trim_spaces();
                } else if i > 0 && (self.too_long(&item) || multiline) {
                    self.keyword("", -INDENT_STD, INDENT_STD, INDENT_VAR);
                }
                multiline = item[usize::from(leading)..].contains('\n');
            }
            self.buf.push_str(&item);
        }
        Ok(())
    }

    /// `removeStringInfoSpaces`.
    fn trim_spaces(&mut self) {
        let kept = self.buf.trim_end_matches(' ').len();
        self.buf.truncate(kept);
    }

    /// True when the item makes the last line longer than the wrap column.
    fn too_long(&self, item: &str) -> bool {
        let line = self.buf.rsplit('\n').next().map_or(0, str::len);
        usize::try_from(self.wrap).is_ok_and(|wrap| line + item.len() > wrap)
    }

    /// `get_from_clause`: the items of `FROM`. An item after the first starts a new line when the line gets longer than the wrap column.
    fn clause_from(&mut self, query: &Query) -> Result<()> {
        for (i, item) in query.from.iter().enumerate() {
            if i == 0 {
                self.keyword(" FROM ", -INDENT_STD, INDENT_STD, 2);
                self.range_item(item, query)?;
                continue;
            }
            self.buf.push_str(", ");
            let outer = std::mem::take(&mut self.buf);
            self.range_item(item, query)?;
            let text = std::mem::replace(&mut self.buf, outer);
            if self.wrap >= 0 {
                if text.starts_with('\n') {
                    self.trim_spaces();
                } else if self.too_long(&text) {
                    self.keyword("", -INDENT_STD, INDENT_STD, INDENT_VAR);
                }
            }
            self.buf.push_str(&text);
        }
        Ok(())
    }

    /// `get_from_clause_item`: a relation with its alias and the names of its columns, or a join.
    fn range_item(&mut self, item: &FromItem, query: &Query) -> Result<()> {
        let index = match item {
            FromItem::Relation(index) => *index,
            FromItem::Join(join) => return self.join(join, query),
        };
        let rel = query
            .relations
            .get(index)
            .ok_or_else(|| Error::internal(format!("no relation {index} in the query")))?;
        let lateral = rel.lateral
            || rel.function.as_ref().is_some_and(|f| f.calls.iter().any(|(c, _)| refers_to(c, 0)));
        if lateral {
            self.buf.push_str("LATERAL ");
        }
        let mut coldef = false;
        if let Some(cte) = &rel.cte {
            self.buf.push_str(&quote_identifier(&cte.name));
        } else if rel.oid != 0 {
            self.buf.push_str(&relation_name(rel.oid, self.session)?);
        } else if let Some(table) = rel.json_table() {
            self.json_table(table)?;
        } else if let Some(function) = &rel.function {
            coldef = self.function_item(function, &rel.columns)?;
        } else if let Some(rows) = &rel.values {
            self.buf.push('(');
            self.values(rows)?;
            self.buf.push(')');
        } else if let Some(sub) = &rel.subquery {
            self.buf.push('(');
            self.query_def(sub, None, true, self.indent)?;
            self.buf.push(')');
        }
        let space = self.namespaces.last().ok_or_else(|| Error::internal("no namespace"))?;
        let refname = space.names.get(index).cloned().unwrap_or_default();
        let columns = space.columns.get(index).cloned().unwrap_or_default();
        let print_aliases = space.print_aliases.get(index).copied().unwrap_or(false);
        // `get_rte_alias`: a subquery, a function and a `VALUES` list always show their alias, and `JSON_TABLE` shows it only when the query gives one. A table, a view and a query of `WITH` show it when the query gives one or when the name is not the name of the relation.
        let show = rel.alias.is_some()
            || print_aliases
            || match &rel.cte {
                Some(cte) => cte.name != refname,
                None if rel.json_table().is_some() => false,
                None if rel.oid == 0 => true,
                None => {
                    reg::class_name(rel.oid, self.session).is_none_or(|(name, _)| name != refname)
                }
            };
        if show {
            self.buf.push(' ');
            self.buf.push_str(&quote_identifier(&refname));
        }
        if coldef {
            self.coldef(&columns, &rel.columns)?;
        } else if print_aliases && !columns.is_empty() {
            // `get_column_alias_list` leaves out a dropped column.
            let names: Vec<String> =
                columns.iter().filter(|c| !c.is_empty()).map(|c| quote_identifier(c)).collect();
            self.buf.push_str(&format!("({})", names.join(", ")));
        }
        // `TABLESAMPLE` goes after the alias.
        if let Some(sample) = &rel.tablesample {
            self.tablesample(sample)?;
        }
        Ok(())
    }

    /// `get_tablesample_def`: ` TABLESAMPLE method (args)`, then `REPEATABLE (seed)` when the query gives a seed.
    fn tablesample(&mut self, sample: &TableSample) -> Result<()> {
        let proc = builtin::proc_by_oid(sample.handler).ok_or_else(|| {
            Error::internal(format!("cache lookup failed for function {}", sample.handler))
        })?;
        let name = if reg::function_visible(proc, self.session) {
            quote_identifier(proc.name)
        } else {
            reg::qualified(reg::namespace_name(proc.namespace, self.session), proc.name)
        };
        self.buf.push_str(&format!(" TABLESAMPLE {name} ("));
        for (i, arg) in sample.args.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            self.expr(arg, false)?;
        }
        self.buf.push(')');
        if let Some(seed) = &sample.repeatable {
            self.buf.push_str(" REPEATABLE (");
            self.expr(seed, false)?;
            self.buf.push(')');
        }
        Ok(())
    }

    /// The calls of a function in `FROM`. The result is true when the column definition list of the one call goes after the alias.
    fn function_item(&mut self, function: &FromFunction, columns: &[Column]) -> Result<bool> {
        let has_coldef = |i: usize| function.coldefs.get(i).copied().unwrap_or(false);
        let mut coldef = false;
        if let [(call, _)] = function.calls.as_slice()
            && (!has_coldef(0) || !function.ordinality)
        {
            self.funccall(call)?;
            coldef = has_coldef(0);
        } else if function.calls.iter().enumerate().all(|(i, (call, _))| {
            !has_coldef(i) && matches!(&call.kind, ExprKind::Func(f) if f.oid == UNNEST)
        }) {
            self.buf.push_str("UNNEST(");
            let mut first = true;
            for (call, _) in &function.calls {
                let ExprKind::Func(f) = &call.kind else { continue };
                for arg in &f.args {
                    if !first {
                        self.buf.push_str(", ");
                    }
                    first = false;
                    self.expr(arg, true)?;
                }
            }
            self.buf.push(')');
        } else {
            self.buf.push_str("ROWS FROM(");
            let mut at = 0;
            for (i, (call, count)) in function.calls.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                self.funccall(call)?;
                let part = columns.get(at..at + count).unwrap_or_default();
                if has_coldef(i) {
                    self.buf.push_str(" AS ");
                    let names: Vec<String> = part.iter().map(|c| c.name.clone()).collect();
                    self.coldef(&names, part)?;
                }
                at += count;
            }
            self.buf.push(')');
        }
        if function.ordinality {
            self.buf.push_str(" WITH ORDINALITY");
        }
        Ok(coldef)
    }

    /// `get_rule_expr_funccall`: a call of a function in `FROM`, or `CAST(... AS type)` for an expression that the syntax of `FROM` does not take as a call.
    fn funccall(&mut self, e: &Expr) -> Result<()> {
        if looks_like_function(e) {
            return self.expr(e, true);
        }
        self.buf.push_str("CAST(");
        self.expr(e, false)?;
        let ty = reg::type_text(e.ty, Some(e.typmod), self.session)?;
        self.buf.push_str(&format!(" AS {ty})"));
        Ok(())
    }

    /// `get_from_clause_coldeflist`: the column definition list of a function, `(name type, ...)`.
    fn coldef(&mut self, names: &[String], columns: &[Column]) -> Result<()> {
        self.buf.push('(');
        for (i, (name, column)) in names.iter().zip(columns).enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            let ty = reg::type_text(column.ty, Some(column.typmod), self.session)?;
            self.buf.push_str(&format!("{} {ty}", quote_identifier(name)));
        }
        self.buf.push(')');
        Ok(())
    }

    /// A join of `FROM`, with the names of `USING` that the namespace chooses. A join with an alias is always in parentheses, and the alias and the names of the columns come after them.
    fn join(&mut self, join: &Join, query: &Query) -> Result<()> {
        let mut relations = Vec::new();
        base_relations(&join.left, &mut relations);
        base_relations(&join.right, &mut relations);
        let space = self.namespaces.last().ok_or_else(|| Error::internal("no namespace"))?;
        let k = space
            .joins
            .iter()
            .position(|j| j.relations == relations)
            .ok_or_else(|| Error::internal("a join with no names"))?;
        let using = space.joins[k].using.clone();
        let paren_right = self.paren
            && !matches!(&join.right, FromItem::Relation(_))
            && !matches!(&join.right, FromItem::Join(right) if right.alias.is_some());
        let parens = !self.paren || join.alias.is_some();
        if parens {
            self.buf.push('(');
        }
        if let Some(space) = self.namespaces.last_mut() {
            space.inside.push(k);
        }
        let done = self.join_body(join, query, paren_right, &using);
        if let Some(space) = self.namespaces.last_mut() {
            space.inside.pop();
        }
        done?;
        if parens {
            self.buf.push(')');
        }
        let space = self.namespaces.last().ok_or_else(|| Error::internal("no namespace"))?;
        let names = &space.joins[k];
        if let Some(name) = &names.name {
            let mut text = format!(" {}", quote_identifier(name));
            if names.print_aliases && !names.columns.is_empty() {
                let columns: Vec<String> = names
                    .columns
                    .iter()
                    .filter(|c| !c.is_empty())
                    .map(|c| quote_identifier(c))
                    .collect();
                text.push_str(&format!("({})", columns.join(", ")));
            }
            self.buf.push_str(&text);
        }
        Ok(())
    }

    /// The two sides of a join and its condition.
    fn join_body(
        &mut self,
        join: &Join,
        query: &Query,
        paren_right: bool,
        using: &[String],
    ) -> Result<()> {
        self.range_item(&join.left, query)?;
        let word = match join.kind {
            JoinKind::Inner if join.on.is_some() => " JOIN ",
            JoinKind::Inner => " CROSS JOIN ",
            JoinKind::Left => " LEFT JOIN ",
            JoinKind::Full => " FULL JOIN ",
            JoinKind::Right => " RIGHT JOIN ",
            JoinKind::Semi | JoinKind::Anti | JoinKind::RightSemi | JoinKind::RightAnti => " JOIN ",
        };
        self.keyword(word, -INDENT_STD, INDENT_STD, INDENT_JOIN);
        if paren_right {
            self.buf.push('(');
        }
        self.range_item(&join.right, query)?;
        if paren_right {
            self.buf.push(')');
        }
        if !join.using.is_empty() {
            let names: Vec<String> = using.iter().map(|n| quote_identifier(n)).collect();
            self.buf.push_str(&format!(" USING ({})", names.join(", ")));
            if let Some(alias) = &join.using_alias {
                self.buf.push_str(&format!(" AS {}", quote_identifier(alias)));
            }
        } else if let Some(on) = &join.on {
            self.buf.push_str(" ON ");
            self.open();
            self.expr(on, false)?;
            self.close();
        } else if join.kind != JoinKind::Inner {
            self.buf.push_str(" ON TRUE");
        }
        Ok(())
    }

    /// `get_setop_query` for the operation of the query.
    fn set_op(&mut self, op: &SetOp, query: &Query, result: Option<&[String]>) -> Result<()> {
        let paren_left =
            matches!(&op.left, SetTree::Op(left) if left.kind != op.kind || left.all != op.all);
        let mut sub = 0;
        if paren_left {
            self.buf.push('(');
            sub = INDENT_STD;
            self.keyword("", sub, 0, 0);
        }
        self.set_tree(&op.left, query, result)?;
        self.keyword(if paren_left { ") " } else { "" }, -sub, 0, 0);
        self.buf.push_str(match op.kind {
            SetKind::Union => "UNION ",
            SetKind::Intersect => "INTERSECT ",
            SetKind::Except => "EXCEPT ",
        });
        if op.all {
            self.buf.push_str("ALL ");
        }
        let paren_right = matches!(op.right, SetTree::Op(_));
        sub = 0;
        if paren_right {
            self.buf.push('(');
            sub = INDENT_STD;
        }
        self.keyword("", sub, 0, 0);
        let saved = std::mem::replace(&mut self.names_visible, false);
        self.set_tree(&op.right, query, result)?;
        self.names_visible = saved;
        self.indent -= sub;
        if paren_right {
            self.keyword(")", 0, 0, 0);
        }
        Ok(())
    }

    /// `get_setop_query` for a part of the operation: a `SELECT`, in parentheses when it has `ORDER BY`, `OFFSET`, `LIMIT` or an operation, or another operation.
    fn set_tree(&mut self, tree: &SetTree, query: &Query, result: Option<&[String]>) -> Result<()> {
        let index = match tree {
            SetTree::Op(op) => return self.set_op(op, query, result),
            SetTree::Leaf(index) => *index,
        };
        let sub = query
            .relations
            .get(index)
            .and_then(|r| r.subquery.as_deref())
            .ok_or_else(|| Error::internal("a part of a set operation with no query"))?;
        let paren = !sub.sort.is_empty()
            || sub.offset.is_some()
            || sub.limit.is_some()
            || sub.set_op.is_some();
        // The analyzer casts the columns of each `SELECT` to the types of the operation in place, but PostgreSQL keeps the `SELECT` as the query gives it and casts the rows of the operation.
        let mut sub = sub.clone();
        for target in &mut sub.targets {
            target.expr = target.expr.strip_implicit().clone();
        }
        if paren {
            self.buf.push('(');
        }
        self.query_def(&sub, result, self.names_visible, self.indent)?;
        if paren {
            self.buf.push(')');
        }
        Ok(())
    }

    /// `get_rule_orderby`: the items of `ORDER BY`, each with its direction and the place of the nulls when they are not the default.
    fn order_by(&mut self, items: &[SortGroup], exprs: &[&Expr], force: bool) -> Result<()> {
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            let e = exprs
                .get(item.target)
                .ok_or_else(|| Error::internal("an item of ORDER BY with no expression"))?;
            self.sort_group(e, item.target + 1, force)?;
            let (lt, gt) = sort_operators(e.ty);
            if item.operator == lt {
                if item.nulls_first {
                    self.buf.push_str(" NULLS FIRST");
                }
            } else if item.operator == gt {
                self.buf.push_str(" DESC");
                if !item.nulls_first {
                    self.buf.push_str(" NULLS LAST");
                }
            } else {
                let op = builtin::operator_by_oid(item.operator).ok_or_else(|| {
                    Error::internal(format!("cache lookup failed for operator {}", item.operator))
                })?;
                self.buf.push_str(&format!(" USING {}", op.name));
                self.buf.push_str(if item.nulls_first { " NULLS FIRST" } else { " NULLS LAST" });
            }
        }
        Ok(())
    }

    /// `get_rule_sortgroupclause`: an item of `ORDER BY`, `GROUP BY` or `DISTINCT ON`. `force` shows the number of the column, for the `ORDER BY` of a set operation.
    fn sort_group(&mut self, e: &Expr, number: usize, force: bool) -> Result<()> {
        if force {
            self.buf.push_str(&number.to_string());
            return Ok(());
        }
        if self.merged_name(e).is_some() {
            return self.expr(e, true);
        }
        match &e.kind {
            ExprKind::Const(value) => self.constant(e, value, 1),
            ExprKind::Var(var) if var.attnum != 0 => self.variable(var, true).map(|_| ()),
            _ => {
                let paren = self.paren
                    || matches!(&e.kind, ExprKind::Func(f) if !matches!(f.form, FuncForm::Operator(_)))
                    || matches!(
                        e.kind,
                        ExprKind::Agg(_) | ExprKind::Window(_) | ExprKind::JsonCtor(_)
                    );
                if paren {
                    self.buf.push('(');
                }
                self.expr(e, true)?;
                if paren {
                    self.buf.push(')');
                }
                Ok(())
            }
        }
    }

    /// `get_rule_groupingset`: a grouping set of `GROUP BY`. A simple set with one item has no parentheses when `omit_parens` is true.
    fn grouping_set(&mut self, query: &Query, set: &GroupingSet, omit_parens: bool) -> Result<()> {
        let (open, content, omit_child) = match set {
            GroupingSet::Empty => {
                self.buf.push_str("()");
                return Ok(());
            }
            GroupingSet::Simple(items) => {
                let parens = !omit_parens || items.len() != 1;
                if parens {
                    self.buf.push('(');
                }
                for (i, &target) in items.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(", ");
                    }
                    let e = query
                        .targets
                        .get(target)
                        .map(|t| &t.expr)
                        .ok_or_else(|| Error::internal("a grouping set item has no target"))?;
                    self.sort_group(e, target + 1, false)?;
                }
                if parens {
                    self.buf.push(')');
                }
                return Ok(());
            }
            GroupingSet::Rollup(content) => ("ROLLUP(", content, true),
            GroupingSet::Cube(content) => ("CUBE(", content, true),
            GroupingSet::Sets(content) => ("GROUPING SETS (", content, false),
        };
        self.buf.push_str(open);
        for (i, child) in content.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            self.grouping_set(query, child, omit_child)?;
        }
        self.buf.push(')');
        Ok(())
    }

    /// `get_agg_expr`: a call of an aggregate function, with `DISTINCT`, `ORDER BY`, `WITHIN GROUP` and `FILTER`.
    pub(super) fn aggregate(&mut self, agg: &Aggref) -> Result<()> {
        self.aggregate_as(agg, None)
    }

    /// `get_agg_expr_helper`: a call of an aggregate function. `json` has the name, the options and the kind of a SQL/JSON aggregate, where `JSON_OBJECTAGG` puts ` : ` between its two arguments.
    pub(super) fn aggregate_as(
        &mut self,
        agg: &Aggref,
        json: Option<(&str, &str, bool)>,
    ) -> Result<()> {
        let proc = builtin::proc_by_oid(agg.oid).ok_or_else(|| {
            Error::internal(format!("cache lookup failed for function {}", agg.oid))
        })?;
        match json {
            Some((name, _, _)) => self.buf.push_str(name),
            None => self.buf.push_str(&quote_identifier(proc.name)),
        }
        self.buf.push('(');
        if !agg.distinct.is_empty() {
            self.buf.push_str("DISTINCT ");
        }
        if agg.kind != b'n' {
            // An ordered-set aggregate has no `*` and no `VARIADIC`.
            for (i, arg) in agg.direct.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                self.expr(arg, true)?;
            }
            self.buf.push_str(") WITHIN GROUP (ORDER BY ");
            let exprs: Vec<&Expr> = agg.args.iter().collect();
            self.order_by(&agg.order, &exprs, false)?;
        } else if agg.star {
            self.buf.push('*');
        } else {
            let variadic = agg.variadic && proc.variadic != 0;
            let object = matches!(json, Some((_, _, true)));
            for (i, arg) in agg.args.iter().take(agg.nargs).enumerate() {
                if object && i > 1 {
                    break;
                }
                if i > 0 {
                    self.buf.push_str(if object { " : " } else { ", " });
                }
                if variadic && i + 1 == agg.nargs {
                    self.buf.push_str("VARIADIC ");
                }
                self.expr(arg, true)?;
            }
        }
        if agg.kind == b'n' && !agg.order.is_empty() {
            self.buf.push_str(" ORDER BY ");
            let exprs: Vec<&Expr> = agg.args.iter().collect();
            self.order_by(&agg.order, &exprs, false)?;
        }
        if let Some((_, options, _)) = json {
            self.buf.push_str(options);
        }
        if let Some(filter) = &agg.filter {
            self.buf.push_str(") FILTER (WHERE ");
            self.expr(filter, false)?;
        }
        self.buf.push(')');
        Ok(())
    }

    /// `get_json_constructor` for `JSON_ARRAY` with a subquery. The subquery is the subquery in `FROM` of the query of `SELECT JSON_ARRAYAGG` in [`JsonCtor::func`].
    pub(super) fn json_array_query(&mut self, c: &JsonCtor) -> Result<()> {
        let wrapper = match c.func.as_ref().map(|f| &f.kind) {
            Some(ExprKind::Coalesce(args)) => match args.first().map(|a| &a.kind) {
                Some(ExprKind::SubLink(sub)) => Some(&sub.query),
                _ => None,
            },
            _ => None,
        };
        let Some(wrapper) = wrapper else {
            return Err(Error::internal("JSON_ARRAY with a subquery and no COALESCE"));
        };
        let Some(query) = wrapper.relations.first().and_then(|r| r.subquery.as_deref()) else {
            return Err(Error::internal("JSON_ARRAY with no subquery"));
        };
        self.buf.push_str("JSON_ARRAY(");
        // The subquery is one level below the query of `SELECT JSON_ARRAYAGG`, whose names the subquery does not see.
        let names = vec![String::new(); wrapper.relations.len()];
        self.namespaces.push(Namespace { names, ..Default::default() });
        let done = self.query_def(query, None, false, self.indent);
        self.namespaces.pop();
        done?;
        json_format(c.format, &mut self.buf);
        self.json_ctor_options(c)?;
        self.buf.push(')');
        Ok(())
    }

    /// `get_sublink_expr`: a subquery in an expression.
    pub(super) fn sublink(&mut self, sub: &SubLink) -> Result<()> {
        self.buf.push_str(if sub.kind == SubLinkKind::Array { "ARRAY(" } else { "(" });
        let mut opname = "";
        if let Some(test) = &sub.test {
            // One operator, the operators of `=` or `<>` for a row on the left, or the operators of a comparison of rows.
            let (ops, row) = match &test.kind {
                ExprKind::Bool(_, ops) => (ops.as_slice(), true),
                ExprKind::RowCompare(rc) => (rc.ops.as_slice(), true),
                _ => (std::slice::from_ref(test), false),
            };
            if row {
                self.buf.push('(');
            }
            for (i, op) in ops.iter().enumerate() {
                let ExprKind::Func(Func { form: FuncForm::Operator(opno), args, .. }) = &op.kind
                else {
                    return Err(not_yet("a deparse of a subquery with this test"));
                };
                if i > 0 {
                    self.buf.push_str(", ");
                }
                let left =
                    args.first().ok_or_else(|| Error::internal("a test with no left argument"))?;
                self.expr(left, true)?;
                if i == 0 {
                    opname =
                        builtin::operator_by_oid(*opno).map(|op| op.name).ok_or_else(|| {
                            Error::internal(format!("cache lookup failed for operator {opno}"))
                        })?;
                }
            }
            if row {
                self.buf.push(')');
            }
        }
        let paren = match sub.kind {
            SubLinkKind::Exists => {
                self.buf.push_str("EXISTS ");
                true
            }
            SubLinkKind::Any if opname == "=" => {
                self.buf.push_str(" IN ");
                true
            }
            SubLinkKind::Any => {
                self.buf.push_str(&format!(" {opname} ANY "));
                true
            }
            SubLinkKind::All => {
                self.buf.push_str(&format!(" {opname} ALL "));
                true
            }
            SubLinkKind::RowCompare => {
                self.buf.push_str(&format!(" {opname} "));
                true
            }
            SubLinkKind::Expr | SubLinkKind::Array | SubLinkKind::Multi { .. } => false,
        };
        if paren {
            self.buf.push('(');
        }
        self.query_def(&sub.query, None, false, self.indent)?;
        self.buf.push_str(if paren { "))" } else { ")" });
        Ok(())
    }

    /// `get_func_sql_syntax`: a call in the special syntax of the SQL standard. The result is false for a function that has no special syntax, which then shows as a plain call.
    pub(super) fn sql_syntax(&mut self, e: &Expr, f: &Func) -> Result<bool> {
        let proc = builtin::proc_by_oid(f.oid).ok_or_else(|| {
            Error::internal(format!("cache lookup failed for function {}", f.oid))
        })?;
        let args = f.args.as_slice();
        let text = |at: usize| match args.get(at).map(|a| (&a.kind, a.ty)) {
            Some((ExprKind::Const(value), oid::TEXT)) => value.as_str().map(str::to_string),
            _ => None,
        };
        match (proc.name, args) {
            ("timezone", [zone, value]) => {
                self.buf.push('(');
                self.paren(value, false, e)?;
                self.buf.push_str(" AT TIME ZONE ");
                self.paren(zone, false, e)?;
                self.buf.push(')');
            }
            ("timezone", [value]) => {
                self.buf.push('(');
                self.paren(value, false, e)?;
                self.buf.push_str(" AT LOCAL)");
            }
            ("overlaps", [a, b, c, d]) => {
                self.buf.push_str("((");
                self.expr(a, false)?;
                self.buf.push_str(", ");
                self.expr(b, false)?;
                self.buf.push_str(") OVERLAPS (");
                self.expr(c, false)?;
                self.buf.push_str(", ");
                self.expr(d, false)?;
                self.buf.push_str("))");
            }
            ("extract", [_, value]) => {
                let Some(field) = text(0) else { return Ok(false) };
                self.buf.push_str(&format!("EXTRACT({} FROM ", quote_identifier(&field)));
                self.expr(value, false)?;
                self.buf.push(')');
            }
            ("is_normalized", [value, rest @ ..]) if rest.len() <= 1 => {
                self.buf.push('(');
                self.paren(value, false, e)?;
                self.buf.push_str(" IS");
                if !rest.is_empty() {
                    let Some(form) = text(1) else { return Ok(false) };
                    self.buf.push_str(&format!(" {form}"));
                }
                self.buf.push_str(" NORMALIZED)");
            }
            ("pg_collation_for", [value]) => {
                self.buf.push_str("COLLATION FOR (");
                self.expr(value, false)?;
                self.buf.push(')');
            }
            ("normalize", [value, rest @ ..]) if rest.len() <= 1 => {
                self.buf.push_str("NORMALIZE(");
                self.expr(value, false)?;
                if !rest.is_empty() {
                    let Some(form) = text(1) else { return Ok(false) };
                    self.buf.push_str(&format!(", {form}"));
                }
                self.buf.push(')');
            }
            ("overlay", [value, placing, from, rest @ ..]) if rest.len() <= 1 => {
                self.buf.push_str("OVERLAY(");
                self.expr(value, false)?;
                self.buf.push_str(" PLACING ");
                self.expr(placing, false)?;
                self.buf.push_str(" FROM ");
                self.expr(from, false)?;
                if let [count] = rest {
                    self.buf.push_str(" FOR ");
                    self.expr(count, false)?;
                }
                self.buf.push(')');
            }
            ("position", [value, part]) => {
                self.buf.push_str("POSITION((");
                self.expr(part, false)?;
                self.buf.push_str(") IN (");
                self.expr(value, false)?;
                self.buf.push_str("))");
            }
            ("substring", [value, from, rest @ ..])
                if rest.len() <= 1 && proc.argtypes.get(1) == Some(&oid::INT4) =>
            {
                self.buf.push_str("SUBSTRING(");
                self.expr(value, false)?;
                self.buf.push_str(" FROM ");
                self.expr(from, false)?;
                if let [count] = rest {
                    self.buf.push_str(" FOR ");
                    self.expr(count, false)?;
                }
                self.buf.push(')');
            }
            ("substring", [value, pattern, escape]) => {
                self.buf.push_str("SUBSTRING(");
                self.expr(value, false)?;
                self.buf.push_str(" SIMILAR ");
                self.expr(pattern, false)?;
                self.buf.push_str(" ESCAPE ");
                self.expr(escape, false)?;
                self.buf.push(')');
            }
            ("btrim" | "ltrim" | "rtrim", [value, rest @ ..]) if rest.len() <= 1 => {
                self.buf.push_str(match proc.name {
                    "btrim" => "TRIM(BOTH",
                    "ltrim" => "TRIM(LEADING",
                    _ => "TRIM(TRAILING",
                });
                if let [chars] = rest {
                    self.buf.push(' ');
                    self.expr(chars, false)?;
                }
                self.buf.push_str(" FROM ");
                self.expr(value, false)?;
                self.buf.push(')');
            }
            ("system_user", []) => self.buf.push_str("SYSTEM_USER"),
            _ => return Ok(false),
        }
        Ok(true)
    }
}

/// The values and the names in `names` of the columns of `USING` of a join with no alias that are not columns of one side.
fn merged_columns(join: &Join, names: &[String], out: &mut Vec<(Expr, String)>) {
    if join.alias.is_none() {
        for (value, name) in join.merged.iter().zip(names) {
            if !matches!(value.kind, ExprKind::Var(_)) {
                out.push((value.clone(), name.clone()));
            }
        }
    }
}

/// The number of joins in a part of `FROM`, which `get_query_def` counts as entries of the range table.
fn join_count(item: &FromItem) -> usize {
    match item {
        FromItem::Relation(_) => 0,
        FromItem::Join(join) => 1 + join_count(&join.left) + join_count(&join.right),
    }
}

/// The expression of an item of `ORDER BY`, `GROUP BY` or `DISTINCT ON`.
fn target_expr<'q>(query: &'q Query, item: &SortGroup) -> Result<&'q Expr> {
    query
        .targets
        .get(item.target)
        .map(|t| &t.expr)
        .ok_or_else(|| Error::internal("an item of a clause with no target"))
}

/// The relations of a part of `FROM`, from left to right.
fn base_relations(item: &FromItem, out: &mut Vec<usize>) {
    match item {
        FromItem::Relation(i) => out.push(*i),
        FromItem::Join(join) => {
            base_relations(&join.left, out);
            base_relations(&join.right, out);
        }
    }
}

/// A part of `FROM` as an entry of the range table: a relation by its index, or a join by its index in the joins of the query.
#[derive(Clone, Copy, Debug)]
enum Slot {
    Relation(usize),
    Join(usize),
}

/// A join of `FROM` with its two sides and the relations in it.
#[derive(Debug)]
struct JoinInfo<'q> {
    join: &'q Join,
    left: Slot,
    right: Slot,
    relations: Vec<usize>,
}

/// Adds the joins of a part of `FROM` to `joins`, an outer join before the joins in it, and adds the parts to `order` in the order of the range table, where a join comes after its two sides. The result is the slot of the part.
fn walk_from<'q>(item: &'q FromItem, joins: &mut Vec<JoinInfo<'q>>, order: &mut Vec<Slot>) -> Slot {
    let join = match item {
        FromItem::Relation(i) => {
            order.push(Slot::Relation(*i));
            return Slot::Relation(*i);
        }
        FromItem::Join(join) => join,
    };
    let k = joins.len();
    let (left, right) = (Slot::Relation(0), Slot::Relation(0));
    joins.push(JoinInfo { join, left, right, relations: Vec::new() });
    let left = walk_from(&join.left, joins, order);
    let right = walk_from(&join.right, joins, order);
    let mut relations = Vec::new();
    for side in [left, right] {
        match side {
            Slot::Relation(i) => relations.push(i),
            Slot::Join(j) => relations.extend_from_slice(&joins[j].relations),
        }
    }
    joins[k] = JoinInfo { join, left, right, relations };
    order.push(Slot::Join(k));
    Slot::Join(k)
}

/// The name, or the name with `_1`, `_2` and so on when another relation of the query or of a query outside it has the name, as `set_rtable_names` chooses it.
pub(super) fn unique_name(base: String, used: &mut HashMap<String, u32>) -> String {
    let Some(mut counter) = used.get(&base).copied() else {
        used.insert(base.clone(), 0);
        return base;
    };
    let name = loop {
        counter += 1;
        let name = format!("{base}_{counter}");
        if !used.contains_key(&name) {
            break name;
        }
    };
    used.insert(base, counter);
    used.insert(name.clone(), 0);
    name
}

/// The names of the columns of the relations and the joins of a query while the deparse chooses them, as `colnames` and `parentUsing` of each `deparse_columns` and `using_names` of `deparse_namespace`. `None` is a column with no name yet.
#[derive(Debug)]
struct ColumnNames {
    relations: Vec<Vec<Option<String>>>,
    relation_parents: Vec<Vec<String>>,
    joins: Vec<Vec<Option<String>>>,
    join_parents: Vec<Vec<String>>,
    /// `usingNames` of each join.
    join_using: Vec<Vec<String>>,
    /// `using_names`: the names of `USING` that must be unique in the query.
    using: Vec<String>,
    /// `unique_using`: true when a join with no alias has a column of `USING` that is not a column of one side, as `has_dangerous_join_using` finds.
    unique_using: bool,
    /// For each relation, the place of each column that was not dropped when the analyzer read the query, which the number of a column of a join counts.
    live: Vec<Vec<usize>>,
}

impl ColumnNames {
    fn names(&self, slot: Slot) -> Option<&Vec<Option<String>>> {
        match slot {
            Slot::Relation(i) => self.relations.get(i),
            Slot::Join(k) => self.joins.get(k),
        }
    }

    /// Gives a name to the column with this number from 1 of a part of `FROM`, as a join pushes the names of its columns down to its sides. 0 is no column.
    fn push(&mut self, slot: Slot, number: usize, name: &str) {
        let names = match slot {
            Slot::Relation(i) => self.relations.get_mut(i),
            Slot::Join(k) => self.joins.get_mut(k),
        };
        if let Some(names) = names
            && number > 0
        {
            if names.len() < number {
                names.resize(number, None);
            }
            names[number - 1] = Some(name.to_string());
        }
    }

    /// `make_colname_unique` and `colname_is_unique`: the name, or the name with `_1`, `_2` and so on when another column of the part of `FROM`, a column of `USING` of a join above it, or a name of `USING` that must be unique has it.
    fn unique(&self, name: &str, slot: Slot) -> String {
        let names = self.names(slot).map(Vec::as_slice).unwrap_or_default();
        let parents = match slot {
            Slot::Relation(i) => self.relation_parents.get(i),
            Slot::Join(k) => self.join_parents.get(k),
        };
        let parents = parents.map(Vec::as_slice).unwrap_or_default();
        let taken = |n: &str| {
            names.iter().flatten().any(|t| t == n)
                || parents.iter().any(|t| t == n)
                || self.using.iter().any(|t| t == n)
        };
        if !taken(name) {
            return name.to_string();
        }
        (1..).map(|i| format!("{name}_{i}")).find(|n| !taken(n)).unwrap_or_default()
    }

    /// `set_using_names`: the names of the columns of `USING` of a join and of the joins in it. A join pushes these names down to the columns of its sides, and a join with no alias also pushes down the names that a join above it gives.
    fn using_names(&mut self, joins: &[JoinInfo<'_>], k: usize, parent: &[String]) {
        let Some(info) = joins.get(k) else { return };
        let join = info.join;
        if join.alias.is_none() {
            let pushed = self.joins.get(k).cloned().unwrap_or_default();
            for (i, name) in pushed.iter().enumerate() {
                let Some(name) = name else { continue };
                let (l, r) = join.sources.get(i).copied().unwrap_or_default();
                self.push(info.left, l, name);
                self.push(info.right, r, name);
            }
        }
        let mut parent = parent.to_vec();
        let mut using = Vec::with_capacity(join.using.len());
        for (i, written) in join.using.iter().enumerate() {
            let pushed = self.joins.get(k).and_then(|n| n.get(i)).cloned().flatten();
            let name = match pushed {
                Some(name) => name,
                None => {
                    // A name that the alias of the join gives is better than the name in `USING`.
                    let wanted = match &join.alias {
                        Some(_) => join.aliases.get(i).unwrap_or(written),
                        None => written,
                    };
                    let name = self.unique(wanted, Slot::Join(k));
                    if self.unique_using {
                        self.using.push(name.clone());
                    }
                    self.push(Slot::Join(k), i + 1, &name);
                    name
                }
            };
            let (l, r) = join.sources.get(i).copied().unwrap_or_default();
            self.push(info.left, l, &name);
            self.push(info.right, r, &name);
            parent.push(name.clone());
            using.push(name);
        }
        if let Some(slot) = self.join_using.get_mut(k) {
            *slot = using;
        }
        for side in [info.left, info.right] {
            match side {
                Slot::Relation(i) => {
                    if let Some(slot) = self.relation_parents.get_mut(i) {
                        slot.clone_from(&parent);
                    }
                }
                Slot::Join(j) => {
                    if let Some(slot) = self.join_parents.get_mut(j) {
                        slot.clone_from(&parent);
                    }
                    self.using_names(joins, j, &parent);
                }
            }
        }
    }

    /// `set_join_column_names`: the names of the columns of a join that are not columns of `USING`, after the names of its sides. A join with no alias takes the names of its sides. The result is `printaliases`: true for a join with an alias when a name is not the name of the column of its side.
    fn join_columns(&mut self, joins: &[JoinInfo<'_>], k: usize) -> bool {
        let Some(info) = joins.get(k) else { return false };
        let join = info.join;
        let mut changed = false;
        for i in join.using.len()..join.columns.len() {
            let (l, r) = join.sources.get(i).copied().unwrap_or_default();
            let (side, number) = if l > 0 { (info.left, l) } else { (info.right, r) };
            // The number of a column of a table counts the columns that were not dropped when the analyzer read the query.
            let names = self.names(side).map(Vec::as_slice).unwrap_or_default();
            let place = match side {
                Slot::Relation(r) => {
                    self.live.get(r).and_then(|live| live.get(number.wrapping_sub(1))).copied()
                }
                Slot::Join(_) => number.checked_sub(1),
            };
            let real = place.and_then(|p| names.get(p)).cloned().flatten();
            let Some(real) = real else { continue };
            // A column that `ALTER TABLE ... DROP COLUMN` dropped after the analysis has no name in the join either.
            if real.is_empty() {
                self.push(Slot::Join(k), i + 1, "");
                continue;
            }
            let assigned = self.joins.get(k).and_then(|n| n.get(i)).cloned().flatten();
            let name = match (&join.alias, assigned) {
                (None, _) => real.clone(),
                (Some(_), Some(name)) => name,
                (Some(_), None) => {
                    let wanted = join.aliases.get(i).unwrap_or(&real);
                    self.unique(wanted, Slot::Join(k))
                }
            };
            changed |= name != real;
            self.push(Slot::Join(k), i + 1, &name);
        }
        join.alias.is_some() && changed
    }
}

/// `get_simple_values_rte`: the rows of the query when it is a plain `VALUES` list whose columns keep their names.
fn simple_values<'q>(query: &'q Query, result: Option<&[String]>) -> Option<&'q [Vec<Expr>]> {
    let [rel] = query.relations.as_slice() else { return None };
    let rows = rel.values.as_deref()?;
    if query.targets.len() != rel.columns.len() {
        return None;
    }
    for (i, (target, column)) in query.targets.iter().zip(&rel.columns).enumerate() {
        let name = result.and_then(|r| r.get(i)).unwrap_or(&target.name);
        if target.junk || *name != column.name {
            return None;
        }
    }
    Some(rows)
}

/// `contain_vars_of_level`: true when the expression has a column of the query at this level above it, also in a subquery.
fn refers_to(e: &Expr, level: usize) -> bool {
    match &e.kind {
        ExprKind::Var(var) => var.levels_up == level,
        ExprKind::SubLink(sub) => {
            sub.test.as_ref().is_some_and(|t| refers_to(t, level))
                || sub.query.exprs().into_iter().any(|x| refers_to(x, level + 1))
        }
        _ => e.children().into_iter().any(|c| refers_to(c, level)),
    }
}

/// `pg_get_viewdef_worker`: the query of the view and a `;`, or null when the relation is not a view.
fn viewdef(oid: u32, paren: bool, wrap: i32, session: &dyn Session) -> Result<Value> {
    let env = reg::SessionEnv(session);
    let Some(query) = rupg_analyze::view_query(&env, oid)? else { return Ok(Value::Null) };
    let rel = session
        .catalog()
        .and_then(|c| c.relation(oid))
        .ok_or_else(|| Error::internal(format!("cache lookup failed for relation {oid}")))?;
    let result: Vec<String> = rel.columns.iter().map(|c| c.name.clone()).collect();
    let mut d = Deparser::new(session, paren, wrap);
    d.query_def(&query, Some(&result), true, 0)?;
    d.buf.push(';');
    Ok(Value::text(d.buf))
}

/// The text of a `CREATE VIEW` statement that makes the view `name` again with the query and the names of its columns, as `pg_get_viewdef_worker` gives the query. The catalog keeps the text of a view, and the text names the objects that the query reads, so `ALTER ... RENAME` writes the text again from the query of the old names.
///
/// # Errors
///
/// The errors of the deparse.
pub fn view_statement(
    query: &Query,
    name: &str,
    columns: &[String],
    session: &dyn Session,
) -> Result<String> {
    let mut d = Deparser::new(session, false, 0);
    d.query_def(query, Some(columns), true, 0)?;
    Ok(format!("CREATE VIEW {} AS {}", quote_identifier(name), d.buf))
}

/// The pretty flags and the wrap column of a call from its second argument: none for `pg_get_viewdef(oid)`, a `boolean` for `pg_get_viewdef(oid, bool)`, and the wrap column for `pg_get_viewdef(oid, int4)`, which also sets `PRETTYFLAG_PAREN`.
fn flags(args: &[Value]) -> Result<(bool, i32)> {
    match args.get(1) {
        None => Ok((false, 0)),
        Some(Value::Bool(pretty)) => Ok((*pretty, 0)),
        Some(value) => {
            let wrap = value.as_i64().ok_or_else(bad_value)?;
            Ok((true, i32::try_from(wrap).map_err(|_| bad_value())?))
        }
    }
}

/// `pg_get_viewdef(oid)`, `pg_get_viewdef(oid, bool)` and `pg_get_viewdef(oid, int4)`.
pub(super) fn get_viewdef(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    let (paren, wrap) = flags(args)?;
    viewdef(oid, paren, wrap, call.session)
}

/// `pg_get_viewdef(text)` and `pg_get_viewdef(text, bool)`: the view of a name that `search_path` finds.
pub(super) fn get_viewdef_name(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let text = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    let names = qualified_name_list(text).map_err(type_error)?;
    let oid = reg::relation_oid(&names, call.session)?;
    let (paren, wrap) = flags(args)?;
    viewdef(oid, paren, wrap, call.session)
}

/// `pg_get_ruledef(oid)` and `pg_get_ruledef(oid, bool)`: the `CREATE RULE` statement of a rule, as `make_ruledef` makes it, or null when no rule has the OID. The `_RETURN` rule of a view is `ON SELECT DO INSTEAD` with the query of the view.
pub(super) fn get_ruledef(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    let pretty = pretty_arg(args, 1)?;
    let Some(catalog) = call.session.catalog() else { return Ok(Value::Null) };
    if let Some(rule) = catalog.rule(oid) {
        return ruledef(rule, pretty, call.session).map(Value::Text);
    }
    let view = catalog.relations().find(|r| r.view.as_ref().is_some_and(|v| v.rule == oid));
    let Some(view) = view.map(|r| r.oid) else { return Ok(Value::Null) };
    let Value::Text(query) = viewdef(view, pretty, 0, call.session)? else {
        return Ok(Value::Null);
    };
    let name = if pretty {
        relation_name(view, call.session)?
    } else {
        qualified_relation_name(view, call.session)?
    };
    let rule = quote_identifier("_RETURN");
    Ok(Value::Text(format!("CREATE RULE {rule} AS\n    ON SELECT TO {name} DO INSTEAD {query}")))
}

/// `make_ruledef` for a rule of `CREATE RULE`. The condition reads the names of the first action, and `DO INSTEAD NOTHING` has the query of `NOTHING` as its action.
fn ruledef(rule: &rupg_catalog::Rule, pretty: bool, session: &dyn Session) -> Result<String> {
    let env = reg::SessionEnv(session);
    let tree = rupg_analyze::rule_tree(&env, rule.oid)?
        .ok_or_else(|| Error::internal(format!("cache lookup failed for rule {}", rule.oid)))?;
    if tree.actions.iter().any(|a| a.write.is_some()) {
        return Err(not_yet("a deparse of a rule action INSERT, UPDATE or DELETE"));
    }
    let event = match rule.event {
        b'1' => "SELECT",
        b'2' => "UPDATE",
        b'3' => "INSERT",
        _ => "DELETE",
    };
    let name = if pretty {
        relation_name(rule.relation, session)?
    } else {
        qualified_relation_name(rule.relation, session)?
    };
    let mut d = Deparser::new(session, pretty, 0);
    d.buf = format!("CREATE RULE {} AS\n    ON {event} TO {name}", quote_identifier(&rule.name));
    if let Some(qual) = &tree.qual {
        d.buf.push_str("\n   WHERE ");
        let context = tree.actions.first().unwrap_or(&tree.base);
        let space = d.namespace(context);
        d.prefix = context.relations.len() != 1;
        d.indent = INDENT_STD;
        d.namespaces.push(space);
        d.expr(qual, false)?;
        d.namespaces.pop();
        d.indent = 0;
        d.prefix = false;
    }
    d.buf.push_str(" DO ");
    if rule.instead {
        d.buf.push_str("INSTEAD ");
    }
    match tree.actions.as_slice() {
        [] => d.buf.push_str("NOTHING;"),
        [action] => {
            d.query_def(action, None, true, 0)?;
            d.buf.push(';');
        }
        actions => {
            d.buf.push('(');
            for action in actions {
                d.query_def(action, None, true, 0)?;
                d.buf.push_str(";\n");
            }
            d.buf.push_str(");");
        }
    }
    Ok(d.buf)
}

/// `pg_get_triggerdef(oid)` and `pg_get_triggerdef(oid, bool)`: the `CREATE TRIGGER` statement of a trigger, as `pg_get_triggerdef_worker` makes it, or null when no trigger has the OID.
pub(super) fn get_triggerdef(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    let pretty = pretty_arg(args, 1)?;
    let session = call.session;
    let Some(trigger) = session.catalog().and_then(|c| c.trigger(oid)) else {
        return Ok(Value::Null);
    };
    let kind = trigger.kind;
    let constraint = if trigger.constraint != 0 { "CONSTRAINT " } else { "" };
    let mut d = Deparser::new(session, pretty, 0);
    d.buf = format!("CREATE {constraint}TRIGGER {} ", quote_identifier(&trigger.name));
    d.buf.push_str(if kind & TRIGGER_BEFORE != 0 {
        "BEFORE"
    } else if kind & TRIGGER_INSTEAD != 0 {
        "INSTEAD OF"
    } else {
        "AFTER"
    });
    let mut events = 0;
    let mut event = |d: &mut Deparser<'_>, name: &str| {
        d.buf.push_str(if events > 0 { " OR " } else { " " });
        d.buf.push_str(name);
        events += 1;
    };
    if kind & TRIGGER_INSERT != 0 {
        event(&mut d, "INSERT");
    }
    if kind & TRIGGER_DELETE != 0 {
        event(&mut d, "DELETE");
    }
    if kind & TRIGGER_UPDATE != 0 {
        event(&mut d, "UPDATE");
        if !trigger.columns.is_empty() {
            let names = column_names(trigger.relation, session).unwrap_or_default();
            let columns: Vec<String> = trigger
                .columns
                .iter()
                .map(|&n| {
                    let at = usize::try_from(n - 1).unwrap_or(usize::MAX);
                    names.get(at).map_or_else(String::new, |n| quote_identifier(n))
                })
                .collect();
            d.buf.push_str(" OF ");
            d.buf.push_str(&columns.join(", "));
        }
    }
    if kind & TRIGGER_TRUNCATE != 0 {
        event(&mut d, "TRUNCATE");
    }
    let name = if pretty {
        relation_name(trigger.relation, session)?
    } else {
        qualified_relation_name(trigger.relation, session)?
    };
    d.buf.push_str(&format!(" ON {name} "));
    if trigger.constraint != 0 {
        if trigger.constraint_relation != 0 {
            let from = relation_name(trigger.constraint_relation, session)?;
            d.buf.push_str(&format!("FROM {from} "));
        }
        if !trigger.deferrable {
            d.buf.push_str("NOT ");
        }
        d.buf.push_str("DEFERRABLE INITIALLY ");
        d.buf.push_str(if trigger.deferred { "DEFERRED " } else { "IMMEDIATE " });
    }
    if trigger.old_table.is_some() || trigger.new_table.is_some() {
        d.buf.push_str("REFERENCING ");
        if let Some(old) = &trigger.old_table {
            d.buf.push_str(&format!("OLD TABLE AS {} ", quote_identifier(old)));
        }
        if let Some(new) = &trigger.new_table {
            d.buf.push_str(&format!("NEW TABLE AS {} ", quote_identifier(new)));
        }
    }
    d.buf.push_str(if kind & TRIGGER_ROW != 0 { "FOR EACH ROW " } else { "FOR EACH STATEMENT " });
    let env = reg::SessionEnv(session);
    if let Some((base, qual)) = rupg_analyze::trigger_when(&env, oid)? {
        d.buf.push_str("WHEN (");
        let space = d.namespace(&base);
        d.prefix = true;
        d.indent = INDENT_STD;
        d.namespaces.push(space);
        d.expr(&qual, false)?;
        d.namespaces.pop();
        d.indent = 0;
        d.prefix = false;
        d.buf.push_str(") ");
    }
    let proc = builtin::proc_by_oid(trigger.function).ok_or_else(|| {
        Error::internal(format!("cache lookup failed for function {}", trigger.function))
    })?;
    let function = if reg::function_visible(proc, session) {
        quote_identifier(proc.name)
    } else {
        reg::qualified(reg::namespace_name(proc.namespace, session), proc.name)
    };
    // `simple_quote_literal` with `standard_conforming_strings` on doubles only the quote.
    let args: Vec<String> =
        trigger.args.iter().map(|a| format!("'{}'", a.replace('\'', "''"))).collect();
    d.buf.push_str(&format!("EXECUTE FUNCTION {function}({})", args.join(", ")));
    Ok(Value::text(d.buf))
}
