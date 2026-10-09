//! `pg_get_viewdef` and the parts of `ruleutils.c` that show a query: `get_query_def`, the target list, `FROM`, the set operations, `ORDER BY`, the aggregate calls, the subqueries and the calls in the special syntax of the SQL standard.
//!
//! rupg keeps the text of the statement that made a view, not its query tree. So `pg_get_viewdef` analyzes the text again, with the `search_path` that the statement had, and shows the query as PostgreSQL shows the query tree that it keeps.

use std::collections::HashMap;

use rupg_analyze::{
    Column, FromFunction, FromItem, Join, JoinKind, Query, SetKind, SetOp, SetTree,
};
use rupg_types::qualified_name_list;

use super::{
    Aggref, Deparser, Error, Expr, ExprKind, Func, FuncForm, INDENT_STD, INDENT_VAR, Namespace,
    Result, SortGroup, SubLink, SubLinkKind, Value, WindowClause, WindowFunc, builtin,
    looks_like_function, oid, pretty_arg, qualified_relation_name, quote_identifier, reg,
    relation_name, sort_operators,
};
use crate::{Call, Session, bad_value, not_yet, type_error};

/// `PRETTYINDENT_JOIN`: the indent of a `JOIN` keyword after the start of its line.
const INDENT_JOIN: i32 = 4;
/// `F_UNNEST_ANYARRAY`: the function `unnest(anyarray)`.
const UNNEST: u32 = 2331;

impl Deparser<'_> {
    /// `get_query_def`: the query with the names of its relations, in the queries outside it. `result` has the names of the columns of the result when the caller knows them, and `indent` is the indent of the query.
    fn query_def(
        &mut self,
        query: &Query,
        result: Option<&[String]>,
        names_visible: bool,
        indent: i32,
    ) -> Result<()> {
        let space = self.namespace(query);
        let joins: usize = query.from.iter().map(join_count).sum();
        let prefix = !self.namespaces.is_empty() || query.relations.len() + joins != 1;
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

    /// `set_rtable_names` and `set_relation_column_names`: a name for each relation of the query that no other relation of the query or of the queries outside it has, and the names of the columns of each relation.
    fn namespace(&self, query: &Query) -> Namespace {
        let mut used: HashMap<String, u32> = HashMap::new();
        for space in &self.namespaces {
            for name in space.names.iter().filter(|n| !n.is_empty()) {
                used.insert(name.clone(), 0);
            }
        }
        let mut space = Namespace::default();
        for rel in &query.relations {
            let base = match reg::class_name(rel.oid, self.session) {
                Some((name, _)) if rel.alias.is_none() && rel.oid != 0 => name.to_string(),
                _ => rel.name.clone(),
            };
            let name = match used.get(&base).copied() {
                None => {
                    used.insert(base.clone(), 0);
                    base
                }
                Some(mut counter) => {
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
            };
            let given: &[String] = rel.alias.as_deref().unwrap_or_default();
            let mut columns: Vec<String> = Vec::with_capacity(rel.columns.len());
            let mut changed = false;
            for (i, column) in rel.columns.iter().enumerate() {
                let wanted = given.get(i).unwrap_or(&column.name);
                // The real name of a column of a table or a view is the name in the catalog. For another relation, it is the name that the alias gives.
                let real = if rel.oid != 0 { &column.name } else { wanted };
                let name = unique_column(wanted, &columns);
                changed |= name != *real;
                columns.push(name);
            }
            let print = if rel.oid != 0 {
                changed
            } else {
                rel.function.is_some() || !given.is_empty() || changed
            };
            space.names.push(name);
            space.columns.push(columns);
            space.print_aliases.push(print);
        }
        for item in &query.from {
            merged_columns(item, &mut space.merged);
        }
        space.relations.clone_from(&query.relations);
        space
    }

    /// The name of a column of `USING` that is not a column of one side, when the expression is its value in this query or in a query outside it.
    pub(super) fn merged_name(&self, e: &Expr) -> Option<String> {
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

    /// `get_basic_select_query`: a `SELECT` with no set operation, or a plain `VALUES` list.
    fn basic_select(&mut self, query: &Query, result: Option<&[String]>) -> Result<()> {
        self.indent += INDENT_STD;
        self.buf.push(' ');
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
        if !query.group.is_empty() {
            self.keyword(" GROUP BY ", -INDENT_STD, INDENT_STD, 1);
            let saved = std::mem::replace(&mut self.in_group_by, true);
            for (i, item) in query.group.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                let e = target_expr(query, item)?;
                self.sort_group(e, item.target + 1, false)?;
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
        }
        self.buf.push(')');
        Ok(())
    }

    /// `get_windowfunc_expr`: a call of a window function, then `OVER` and the name of its window or the window in parentheses.
    pub(super) fn window_func(&mut self, w: &WindowFunc) -> Result<()> {
        let proc = builtin::proc_by_oid(w.oid).ok_or_else(|| {
            Error::internal(format!("cache lookup failed for function {}", w.oid))
        })?;
        self.buf.push_str(&quote_identifier(proc.name));
        self.buf.push('(');
        if w.star {
            self.buf.push('*');
        } else {
            for (i, arg) in w.args.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                self.expr(arg, true)?;
            }
        }
        self.buf.push_str(") OVER ");
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
                self.expr(value, false)?;
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
        if rel.oid != 0 {
            self.buf.push_str(&relation_name(rel.oid, self.session)?);
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
        // `get_rte_alias`: a subquery, a function and a `VALUES` list always show their alias, and a table or a view shows it when the query gives one or when the name is not the name of the relation.
        let show = rel.alias.is_some()
            || print_aliases
            || rel.oid == 0
            || reg::class_name(rel.oid, self.session).is_none_or(|(name, _)| name != refname);
        if show {
            self.buf.push(' ');
            self.buf.push_str(&quote_identifier(&refname));
        }
        if coldef {
            self.coldef(&columns, &rel.columns)?;
        } else if print_aliases && !columns.is_empty() {
            let names: Vec<String> = columns.iter().map(|c| quote_identifier(c)).collect();
            self.buf.push_str(&format!("({})", names.join(", ")));
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

    /// A join of `FROM`. A join with an alias needs the names of the columns of the join, which the engine does not choose yet.
    fn join(&mut self, join: &Join, query: &Query) -> Result<()> {
        if join.alias.is_some() {
            return Err(not_yet("a deparse of a join with an alias"));
        }
        let paren_right = self.paren && !matches!(join.right, FromItem::Relation(_));
        self.open();
        self.range_item(&join.left, query)?;
        let word = match join.kind {
            JoinKind::Inner if join.on.is_some() => " JOIN ",
            JoinKind::Inner => " CROSS JOIN ",
            JoinKind::Left => " LEFT JOIN ",
            JoinKind::Full => " FULL JOIN ",
            JoinKind::Right => " RIGHT JOIN ",
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
            let names: Vec<String> = join.using.iter().map(|n| quote_identifier(n)).collect();
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
        self.close();
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
                    || matches!(e.kind, ExprKind::Agg(_) | ExprKind::Window(_));
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

    /// `get_agg_expr`: a call of an aggregate function, with `DISTINCT`, `ORDER BY` and `FILTER`.
    pub(super) fn aggregate(&mut self, agg: &Aggref) -> Result<()> {
        let proc = builtin::proc_by_oid(agg.oid).ok_or_else(|| {
            Error::internal(format!("cache lookup failed for function {}", agg.oid))
        })?;
        self.buf.push_str(&quote_identifier(proc.name));
        self.buf.push('(');
        if !agg.distinct.is_empty() {
            self.buf.push_str("DISTINCT ");
        }
        if agg.star {
            self.buf.push('*');
        } else {
            let variadic = agg.variadic && proc.variadic != 0;
            for (i, arg) in agg.args.iter().take(agg.nargs).enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                if variadic && i + 1 == agg.nargs {
                    self.buf.push_str("VARIADIC ");
                }
                self.expr(arg, true)?;
            }
        }
        if !agg.order.is_empty() {
            self.buf.push_str(" ORDER BY ");
            let exprs: Vec<&Expr> = agg.args.iter().collect();
            self.order_by(&agg.order, &exprs, false)?;
        }
        if let Some(filter) = &agg.filter {
            self.buf.push_str(") FILTER (WHERE ");
            self.expr(filter, false)?;
        }
        self.buf.push(')');
        Ok(())
    }

    /// `get_sublink_expr`: a subquery in an expression.
    pub(super) fn sublink(&mut self, sub: &SubLink) -> Result<()> {
        self.buf.push_str(if sub.kind == SubLinkKind::Array { "ARRAY(" } else { "(" });
        let mut opname = "";
        if let Some(test) = &sub.test {
            // One operator, or the operators of `=` or `<>` for a row on the left.
            let (ops, row) = match &test.kind {
                ExprKind::Bool(_, ops) => (ops.as_slice(), true),
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
            SubLinkKind::Expr | SubLinkKind::Array => false,
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

/// The values and the names of the columns of `USING` of the joins with no alias in a part of `FROM` that are not columns of one side.
fn merged_columns(item: &FromItem, out: &mut Vec<(Expr, String)>) {
    let FromItem::Join(join) = item else { return };
    merged_columns(&join.left, out);
    merged_columns(&join.right, out);
    if join.alias.is_none() {
        for (value, name) in join.merged.iter().zip(&join.using) {
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

/// `make_colname_unique`: the name, or the name with `_1`, `_2` and so on when another column of the relation has it.
fn unique_column(name: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == name) {
        return name.to_string();
    }
    (1..).map(|i| format!("{name}_{i}")).find(|n| !taken.contains(n)).unwrap_or_default()
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

/// `pg_get_ruledef(oid)` and `pg_get_ruledef(oid, bool)`: the `CREATE RULE` statement of a rule, as `make_ruledef` makes it, or null when no rule has the OID. The only rules of rupg are the `_RETURN` rules of the views, which are `ON SELECT DO INSTEAD` with the query of the view.
pub(super) fn get_ruledef(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    let pretty = pretty_arg(args, 1)?;
    let view = call.session.catalog().and_then(|c| {
        c.relations().find(|r| r.view.as_ref().is_some_and(|v| v.rule == oid)).map(|r| r.oid)
    });
    let Some(view) = view else { return Ok(Value::Null) };
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
