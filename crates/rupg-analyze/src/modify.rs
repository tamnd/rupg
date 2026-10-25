//! `transformUpdateStmt` and `transformDeleteStmt` of `analyze.c`, with `transformUpdateTargetList` of `analyze.c`, `transformMultiAssignRef` of `parse_target.c` and the defaults and the checks of `rewriteTargetListIU`.
//!
//! The target table is the first relation of `FROM`, and the relations of `FROM` or `USING` come after it. The query of an `UPDATE` gives the new row of each row that changes: the expression of each column that `SET` gives, and the old value of each other column. The conditions of the check constraints come after the columns, and the ID of the version of the old row comes last. The query of a `DELETE` gives only the ID of the version of each row that goes.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{
    CurrentOfExpr, DeleteStmt, Node, RangeVar, ResTarget, ReturningClause, SubLinkType, UpdateStmt,
};
use rupg_types::{Value, oid};

use crate::Analyzer;
use crate::agg::Kind;
use crate::coerce::AtOpt;
use crate::expr::{CurrentOf, Expr, ExprKind, SubLink, SubLinkKind, Var};
use crate::from::{Column, FromItem, TABLE_OID_ATTNUM};
use crate::insert::{
    Generated, Item, Write, WriteKind, default_part, merge_parts, not_yet, targets, with_checks,
};
use crate::select::{Query, Target};
use crate::sort::raw_location;
use crate::typename::place;

/// The numbers of the columns that the items of `SET` name, in the order of the list. The analysis of the items checks the names.
pub(crate) fn set_columns(list: &rupg_sql::nodes::List, columns: &[Column]) -> Vec<i16> {
    let names = list.iter().filter_map(|node| match node {
        Some(Node::ResTarget(rt)) => rt.name.as_deref(),
        _ => None,
    });
    names
        .filter_map(|name| columns.iter().position(|c| c.name == name))
        .map(|at| i16::try_from(at + 1).unwrap_or(i16::MAX))
        .collect()
}

/// `ExecGetAllUpdatedCols`: the columns `updated` that `SET` gives, then the generated columns of `generated` that read one of them, as `ExecGetExtraUpdatedCols` adds them for the triggers of `UPDATE OF`.
pub(crate) fn with_extra_updated(mut updated: Vec<i16>, generated: &[Generated]) -> Vec<i16> {
    let set = updated.clone();
    for (index, expr, _) in generated {
        let attnum = i16::try_from(index + 1).unwrap_or(i16::MAX);
        let reads = expr.find(0, &mut |e, depth| match &e.kind {
            ExprKind::Var(var) if var.levels_up == depth && set.contains(&var.attnum) => Some(()),
            _ => None,
        });
        if reads.is_some() && !updated.contains(&attnum) {
            updated.push(attnum);
        }
    }
    updated
}

/// `SelfItemPointerAttributeNumber`: the attribute number of `ctid`, which reads the ID of the version of a row of the target table.
const CTID_ATTNUM: i16 = -1;

/// The names of the system columns, which `SET` cannot assign.
const SYSTEM_COLUMNS: [&str; 6] = ["tableoid", "ctid", "xmin", "cmin", "xmax", "cmax"];

/// The error of `transformUpdateTargetList` for an item of `SET` whose column the target relation `name` does not have.
pub(crate) fn unknown_set_column(rv: &RangeVar, name: &str, rt: &ResTarget) -> Error {
    let col_name = rt.name.as_deref().unwrap_or_default();
    let at = place(rt.location);
    if SYSTEM_COLUMNS.contains(&col_name) {
        return Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("cannot assign to system column \"{col_name}\""),
        )
        .at_opt(at);
    }
    let mut error = Error::new(
        SqlState::UNDEFINED_COLUMN,
        format!("column \"{col_name}\" of relation \"{name}\" does not exist"),
    );
    let alias = rv.alias.as_ref().and_then(|a| a.aliasname.as_deref());
    if !rt.indirection.is_empty() && alias.or(rv.relname.as_deref()) == Some(col_name) {
        error = error.with_hint("SET target columns cannot be qualified with the relation name.");
    }
    error.at_opt(at)
}

impl Analyzer<'_> {
    /// `transformUpdateStmt`: the query tree of an `UPDATE`.
    pub(crate) fn update(&mut self, s: &UpdateStmt) -> Result<Query> {
        let inverted = std::mem::take(&mut self.rule.inverted);
        self.write_with(s.withClause.as_deref())?;
        let rv = s.relation.as_deref().ok_or_else(|| Error::internal("an UPDATE with no table"))?;
        let (table, name, columns) = self.target_table(rv, WriteKind::Update)?;
        let rows = std::mem::take(&mut self.rule.rows);
        if self.auto_view.is_some() && !inverted.is_empty() {
            return Err(not_yet("a conditional rule DO INSTEAD of an updatable view"));
        }
        // The items of `SET` of an updatable view name the columns of its base relation.
        let (renamed, unknown, fixed) = match self.auto_view_set(rv, &s.targetList, &columns) {
            Some((list, unknown, fixed)) => (Some(list), unknown, fixed),
            None => (None, None, None),
        };
        let list = renamed.as_ref().unwrap_or(&s.targetList);
        self.mark_updated(&set_columns(list, &columns));
        let from = self.target_from(rv, &s.fromClause)?;
        let filter = self.target_where(s.whereClause.as_ref())?;
        let filter = self.auto_view_filter(filter);
        // The items of `SET` must name updatable columns of the view.
        if let Some(error) = unknown.or(fixed) {
            self.set_items(list)?;
            return Err(error);
        }
        let row = self.update_row(rv, table, &name, &columns, list)?;
        if rows {
            // The rows of `*rule*`: the new row, with its generated columns, then the old row.
            let generated = if self.is_partitioned(table) {
                Vec::new()
            } else {
                self.generated_columns(table)?
            };
            let mut targets = targets(&columns, crate::rules::with_generated(row, &generated));
            targets.extend(old_row(&columns));
            let mut query = Query {
                relations: std::mem::take(&mut self.scope.relations),
                from,
                targets,
                filter,
                ..Query::default()
            };
            self.end_write_with(&mut query, s.withClause.as_deref())?;
            self.rule.columns = columns;
            return Ok(query);
        }
        let (returning, carried) =
            self.write_returning(s.returningClause.as_deref(), WriteKind::Update)?;
        let tail = tail_columns(&carried);
        let inherit = self.inherits_target(WriteKind::Update);
        let mut query = Query {
            relations: std::mem::take(&mut self.scope.relations),
            from,
            targets: targets(&columns, row),
            filter,
            barrier: self.auto_view_barrier(),
            ..Query::default()
        };
        query.targets.extend(carried_targets(carried));
        let instead = self.is_view(table);
        // The triggers `INSTEAD OF` of a view get the old row, as the junk column `wholerow` gives it.
        let partitioned = self.is_partitioned(table) || inherit;
        if instead {
            query.targets.extend(old_row(&columns));
        } else {
            if partitioned {
                query.targets.push(table_oid_target());
            }
            query.targets.push(tid_target());
        }
        let checks = if partitioned { Vec::new() } else { self.checks(table)? };
        let names = checks.iter().map(|(n, _)| n.clone()).collect();
        // Each table of the tree of `INHERITS` checks its rows after its triggers.
        // The leaf partitions and the tables of the tree of `INHERITS` compute their generated columns.
        let generated = if partitioned { Vec::new() } else { self.generated_columns(table)? };
        if !inverted.is_empty() {
            let width = columns.len();
            let row: Vec<Expr> = query.targets[..width].iter().map(|t| t.expr.clone()).collect();
            let new = crate::rules::with_generated(row, &generated);
            let old = crate::rules::relation_row(&columns, 0, 0);
            let old = crate::rules::named_row(&columns, old);
            let new = crate::rules::named_row(&columns, new);
            let quals = self.inverted_quals(&inverted, Some(old), Some(new))?;
            query.filter = crate::rules::and_filter(query.filter.take(), quals);
        }
        let recheck = if inherit {
            None
        } else {
            self.recheck(table, WriteKind::Update, &name, &columns, &checks, &generated)
        };
        if !checks.is_empty() || !generated.is_empty() {
            query = with_checks(&name, &columns, query, checks, &generated, &tail);
        }
        let width = columns.len();
        let unique = self.unique_keys(table, &columns)?;
        // Each table of the tree of `INHERITS` fires its foreign keys.
        let foreign = if inherit {
            Vec::new()
        } else {
            self.foreign_keys(table, WriteKind::Update, &columns)?
        };
        query.write = Some(Write {
            kind: WriteKind::Update,
            table,
            name,
            alias: None,
            width,
            checks: names,
            unique,
            returning,
            foreign,
            on_conflict: None,
            merge: None,
            recheck,
            updated: with_extra_updated(set_columns(list, &columns), &generated),
            instead,
            inherit,
            view_check: None,
        });
        self.end_write_with(&mut query, s.withClause.as_deref())?;
        Ok(query)
    }

    /// `transformUpdateTargetList` with the defaults of `rewriteTargetListIU`: the new row of an `UPDATE` or of `ON CONFLICT DO UPDATE` for the items of `SET`. A column that `SET` does not give keeps the value of the column of the target table, which is the relation 0.
    pub(crate) fn update_row(
        &mut self,
        rv: &RangeVar,
        table: u32,
        name: &str,
        columns: &[Column],
        list: &rupg_sql::nodes::List,
    ) -> Result<Vec<Expr>> {
        let items = self.set_items(list)?;
        // `transformUpdateTargetList`: the column of each item, and the expression cast to the type of the column.
        let mut given: Vec<(usize, Option<Expr>)> = Vec::with_capacity(items.len());
        for (rt, item) in items {
            let col_name = rt.name.as_deref().unwrap_or_default();
            let at = place(rt.location);
            let Some(index) = columns.iter().position(|c| c.name == col_name) else {
                return Err(unknown_set_column(rv, name, rt));
            };
            let column = &columns[index];
            let expr = match item {
                Item::Default(_) if !rt.indirection.is_empty() => {
                    return Err(default_part(&rt.indirection, at));
                }
                Item::Default(_) => None,
                Item::Expr(expr) if rt.indirection.is_empty() => Some(self.assign(expr, column)?),
                Item::Expr(expr) => {
                    // The old value of the column is the column of the target table.
                    let mut old = column_var(index, column);
                    old.location = at;
                    Some(self.assign_part(column, &rt.indirection, at, expr, Some(old))?)
                }
            };
            given.push((index, expr));
        }
        // `process_matched_tle` of `rewriteTargetListIU`: a column gets one item, or the items that set parts of the column make one value.
        let mut slots: Vec<Option<Option<Expr>>> = vec![None; columns.len()];
        for (index, expr) in given {
            let name = &columns[index].name;
            slots[index] = Some(match (slots[index].take(), expr) {
                (None, expr) => expr,
                (Some(Some(prior)), Some(expr)) => Some(merge_parts(prior, expr, name)?),
                _ => {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("multiple assignments to same column \"{name}\""),
                    ));
                }
            });
        }
        let mut row = Vec::with_capacity(columns.len());
        for (index, slot) in slots.into_iter().enumerate() {
            let column = &columns[index];
            row.push(match slot {
                Some(Some(_)) if self.identity(table, index) == b'a' => {
                    return Err(Error::new(
                        SqlState::GENERATED_ALWAYS,
                        format!("column \"{}\" can only be updated to DEFAULT", column.name),
                    )
                    .with_detail(format!(
                        "Column \"{}\" is an identity column defined as GENERATED ALWAYS.",
                        column.name
                    )));
                }
                Some(Some(_)) if self.generated(table, index) != 0 => {
                    return Err(Error::new(
                        SqlState::GENERATED_ALWAYS,
                        format!("column \"{}\" can only be updated to DEFAULT", column.name),
                    )
                    .with_detail(format!("Column \"{}\" is a generated column.", column.name)));
                }
                Some(Some(expr)) => expr,
                // A virtual generated column stores a null, and a read of the row computes it.
                Some(None) | None if self.generated(table, index) == b'v' => {
                    let mut null = Expr::new(ExprKind::Const(Value::Null), column.ty);
                    null.typmod = column.typmod;
                    null
                }
                // A generated column keeps its old value until the write computes it.
                Some(None) if self.generated(table, index) != 0 => column_var(index, column),
                Some(None) => self.column_default(table, index, column)?,
                None => column_var(index, column),
            });
        }
        Ok(row)
    }

    /// `transformDeleteStmt`: the query tree of a `DELETE`.
    pub(crate) fn delete(&mut self, s: &DeleteStmt) -> Result<Query> {
        let inverted = std::mem::take(&mut self.rule.inverted);
        self.write_with(s.withClause.as_deref())?;
        let rv = s.relation.as_deref().ok_or_else(|| Error::internal("a DELETE with no table"))?;
        let (table, name, columns) = self.target_table(rv, WriteKind::Delete)?;
        let rows = std::mem::take(&mut self.rule.rows);
        let from = self.target_from(rv, &s.usingClause)?;
        if rows {
            // The rows of `*rule*`: the old row.
            let filter = self.target_where(s.whereClause.as_ref())?;
            let mut query = Query {
                relations: std::mem::take(&mut self.scope.relations),
                from,
                targets: old_row(&columns),
                filter,
                ..Query::default()
            };
            self.end_write_with(&mut query, s.withClause.as_deref())?;
            self.rule.columns = columns;
            return Ok(query);
        }
        let inherit = self.inherits_target(WriteKind::Delete);
        // Each table of the tree of `INHERITS` fires its foreign keys.
        let foreign = if inherit {
            Vec::new()
        } else {
            self.foreign_keys(table, WriteKind::Delete, &columns)?
        };
        let mut filter = self.target_where(s.whereClause.as_ref())?;
        filter = self.auto_view_filter(filter);
        if !inverted.is_empty() {
            if self.auto_view.is_some() {
                return Err(not_yet("a conditional rule DO INSTEAD of an updatable view"));
            }
            let old = crate::rules::named_row(&columns, crate::rules::relation_row(&columns, 0, 0));
            let quals = self.inverted_quals(&inverted, Some(old), None)?;
            filter = crate::rules::and_filter(filter, quals);
        }
        let (returning, carried) =
            self.write_returning(s.returningClause.as_deref(), WriteKind::Delete)?;
        // With `RETURNING`, each row gives the old row and the carried values before the ID of its version.
        let mut targets = Vec::new();
        if returning.is_some() {
            targets = old_row(&columns);
            targets.extend(carried_targets(carried));
        }
        let instead = self.is_view(table);
        if instead {
            targets.extend(old_row(&columns));
        } else {
            if self.is_partitioned(table) || inherit {
                targets.push(table_oid_target());
            }
            targets.push(tid_target());
        }
        let mut query = Query {
            barrier: self.auto_view_barrier(),
            relations: std::mem::take(&mut self.scope.relations),
            from,
            targets,
            filter,
            write: Some(Write {
                kind: WriteKind::Delete,
                table,
                name,
                alias: None,
                width: 0,
                checks: Vec::new(),
                unique: Vec::new(),
                returning,
                foreign,
                on_conflict: None,
                merge: None,
                recheck: None,
                updated: Vec::new(),
                instead,
                inherit,
                view_check: None,
            }),
            ..Query::default()
        };
        self.end_write_with(&mut query, s.withClause.as_deref())?;
        Ok(query)
    }

    /// The query of `RETURNING` of an `UPDATE` or a `DELETE`, and the values that the query of the statement carries for it.
    fn write_returning(
        &mut self,
        clause: Option<&ReturningClause>,
        kind: WriteKind,
    ) -> Result<(Option<Box<Query>>, Vec<Expr>)> {
        match clause {
            None => Ok((None, Vec::new())),
            Some(clause) => {
                let (query, carried) = self.returning(clause, kind)?;
                Ok((Some(Box::new(query)), carried))
            }
        }
    }

    /// `transformCurrentOfExpr`: the condition of `WHERE CURRENT OF` on the target table, which is the relation 0. In a function of PL/pgSQL, a name that is a variable of type `refcursor` gives the name of the cursor as a parameter. An updatable view has no `ctid`, so the rewriter of PostgreSQL rejects the condition.
    fn current_of(&mut self, c: &CurrentOfExpr) -> Result<Expr> {
        if self.auto_view.is_some() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "WHERE CURRENT OF on a view is not implemented",
            ));
        }
        let name = c.cursor_name.as_deref().unwrap_or_default();
        let variable = match self.variables {
            Some(variables) => variables.column(&[name], None)?,
            None => None,
        };
        let cursor = match variable.filter(|v| v.ty == oid::REFCURSOR) {
            Some(v) => Expr::new(ExprKind::Param(v.param), v.ty),
            None => Expr::new(ExprKind::Const(Value::text(name)), oid::TEXT),
        };
        let table = self.scope.relations.first().map_or(0, |r| r.oid);
        let var =
            |attnum, ty| Expr::new(ExprKind::Var(Var { relation: 0, attnum, levels_up: 0 }), ty);
        let args = vec![cursor, var(TABLE_OID_ATTNUM, oid::OID), var(-1, oid::TID)];
        Ok(Expr::new(ExprKind::CurrentOf(Box::new(CurrentOf { table, args })), oid::BOOL))
    }

    /// The target table, then the relations of `FROM` or `USING`, as `setTargetTable` and `transformFromClause` add them. A name that the list gives twice is an error.
    pub(crate) fn target_from(
        &mut self,
        rv: &RangeVar,
        list: &[Option<Node>],
    ) -> Result<Vec<FromItem>> {
        let target = self.target_relation(rv)?;
        let mut items = vec![target];
        items.extend(self.transform_from(list)?);
        Ok(items)
    }

    /// `transformWhereClause` of the statement. `WHERE CURRENT OF` is the condition of [`Analyzer::current_of`].
    pub(crate) fn target_where(&mut self, node: Option<&Node>) -> Result<Option<Expr>> {
        match node {
            None => Ok(None),
            Some(Node::CurrentOfExpr(c)) => self.current_of(c).map(Some),
            Some(node) => {
                let expr = self.with_kind(Kind::Where, |a| a.transform(Some(node)))?;
                Ok(Some(self.coerce_to_boolean(expr, "WHERE")?))
            }
        }
    }

    /// `transformTargetList` for the items of `SET`, with `DEFAULT` kept at the top of an item. An item of `SET (a, b) = ROW(...)` takes its field of the row.
    pub(crate) fn set_items<'s>(
        &mut self,
        list: &'s rupg_sql::nodes::List,
    ) -> Result<Vec<(&'s ResTarget, Item)>> {
        let mut items = Vec::with_capacity(list.len());
        let mut fields: Vec<Option<Item>> = Vec::new();
        // The number of the sub-SELECT items so far, which gives each one its ID, as `subLinkId`.
        let mut subqueries = 0;
        for node in list {
            let Some(Node::ResTarget(rt)) = node else {
                return Err(Error::internal("a SET item that is not ResTarget"));
            };
            let item = match &rt.val {
                Some(Node::MultiAssignRef(multi)) => {
                    if multi.colno == 1 {
                        fields = self.multi_source(
                            multi.source.as_ref(),
                            multi.ncolumns,
                            &mut subqueries,
                        )?;
                    }
                    let at = usize::try_from(multi.colno - 1).unwrap_or(usize::MAX);
                    fields
                        .get_mut(at)
                        .and_then(Option::take)
                        .ok_or_else(|| Error::internal("a SET item with no field"))?
                }
                Some(Node::SetToDefault(d)) => Item::Default(place(d.location)),
                val => {
                    Item::Expr(self.with_kind(Kind::UpdateSource, |a| a.transform(val.as_ref()))?)
                }
            };
            items.push((&**rt, item));
        }
        Ok(items)
    }

    /// `transformMultiAssignRef`: the fields of the source of `SET (a, b) = ...`. A `ROW` expression gives its fields, which can be `DEFAULT`. A sub-SELECT gives a [`SubLinkKind::Multi`] for each of its columns, with the ID that follows `subqueries`.
    fn multi_source(
        &mut self,
        source: Option<&Node>,
        ncolumns: i32,
        subqueries: &mut u32,
    ) -> Result<Vec<Option<Item>>> {
        let at = raw_location(source);
        match source {
            Some(Node::SubLink(link)) if link.subLinkType == SubLinkType::EXPR_SUBLINK => {
                let query =
                    self.with_kind(Kind::UpdateSource, |a| a.sub_analyze(link.subselect.as_ref()))?;
                let columns: Vec<&Expr> =
                    query.targets.iter().filter(|t| !t.junk).map(|t| &t.expr).collect();
                if usize::try_from(ncolumns).ok() != Some(columns.len()) {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        "number of columns does not match number of values",
                    )
                    .at_opt(place(link.location)));
                }
                *subqueries += 1;
                let id = *subqueries;
                let fields = columns
                    .iter()
                    .enumerate()
                    .map(|(column, expr)| {
                        let sub = SubLink {
                            kind: SubLinkKind::Multi { id, column },
                            test: None,
                            query: query.clone(),
                            alternative: None,
                        };
                        Some(Item::Expr(Expr {
                            kind: ExprKind::SubLink(Box::new(sub)),
                            ty: expr.ty,
                            typmod: expr.typmod,
                            location: expr.location,
                        }))
                    })
                    .collect();
                Ok(fields)
            }
            Some(Node::RowExpr(row)) => {
                // `transformRowExpr` with `transformExpressionList`: `t.*` gives the columns of `t`, and the count is of the fields after that.
                let mut fields = Vec::with_capacity(row.args.len());
                for arg in &row.args {
                    if let Some(Node::SetToDefault(d)) = arg {
                        fields.push(Some(Item::Default(place(d.location))));
                        continue;
                    }
                    let exprs = self.with_kind(Kind::UpdateSource, |a| {
                        a.transform_expression_list(std::slice::from_ref(arg))
                    })?;
                    fields.extend(exprs.into_iter().map(|e| Some(Item::Expr(e))));
                }
                if usize::try_from(ncolumns).ok() != Some(fields.len()) {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        "number of columns does not match number of values",
                    )
                    .at_opt(at));
                }
                Ok(fields)
            }
            _ => Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "source for a multiple-column UPDATE item must be a sub-SELECT or ROW() expression",
            )
            .at_opt(at)),
        }
    }
}

/// The targets of the values that the query of an `UPDATE` or a `DELETE` carries for `RETURNING`.
pub(crate) fn carried_targets(carried: Vec<Expr>) -> impl Iterator<Item = Target> {
    carried.into_iter().map(|expr| Target { name: String::new(), expr, origin: None, junk: false })
}

/// The columns of the carried values, which [`with_checks`] keeps after the conditions, then the ID of the version.
fn tail_columns(carried: &[Expr]) -> Vec<Column> {
    let mut tail: Vec<Column> = carried
        .iter()
        .map(|expr| Column {
            name: String::new(),
            ty: expr.ty,
            typmod: expr.typmod,
            not_null: false,
        })
        .collect();
    tail.push(Column { name: "ctid".to_string(), ty: oid::INT8, typmod: -1, not_null: false });
    tail
}

/// The targets that read each column of the old row of the target table.
pub(crate) fn old_row(columns: &[Column]) -> Vec<Target> {
    let row = columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let attnum = i16::try_from(index + 1).unwrap_or(i16::MAX);
            let mut expr =
                Expr::new(ExprKind::Var(Var { relation: 0, attnum, levels_up: 0 }), column.ty);
            expr.typmod = column.typmod;
            expr
        })
        .collect();
    targets(columns, row)
}

/// The target that reads the ID of the version of the row of the target table.
pub(crate) fn tid_target() -> Target {
    let var = Var { relation: 0, attnum: CTID_ATTNUM, levels_up: 0 };
    Target {
        name: "ctid".to_string(),
        expr: Expr::new(ExprKind::Var(var), oid::INT8),
        origin: None,
        junk: false,
    }
}

/// The target that reads the OID of the partition of the row of the target table, a partitioned table.
pub(crate) fn table_oid_target() -> Target {
    let var = Var { relation: 0, attnum: TABLE_OID_ATTNUM, levels_up: 0 };
    Target {
        name: "tableoid".to_string(),
        expr: Expr::new(ExprKind::Var(var), oid::OID),
        origin: None,
        junk: false,
    }
}

/// The column of the target table, which is the relation 0, with the index `index` from 0.
pub(crate) fn column_var(index: usize, column: &Column) -> Expr {
    let attnum = i16::try_from(index + 1).unwrap_or(i16::MAX);
    let mut expr = Expr::new(ExprKind::Var(Var { relation: 0, attnum, levels_up: 0 }), column.ty);
    expr.typmod = column.typmod;
    expr
}
