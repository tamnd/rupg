//! `transformUpdateStmt` and `transformDeleteStmt` of `analyze.c`, with `transformUpdateTargetList` of `analyze.c`, `transformMultiAssignRef` of `parse_target.c` and the defaults and the checks of `rewriteTargetListIU`.
//!
//! The target table is the first relation of `FROM`, and the relations of `FROM` or `USING` come after it. The query of an `UPDATE` gives the new row of each row that changes: the expression of each column that `SET` gives, and the old value of each other column. The conditions of the check constraints come after the columns, and the ID of the version of the old row comes last. The query of a `DELETE` gives only the ID of the version of each row that goes.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{
    DeleteStmt, Node, RangeVar, ResTarget, ReturningClause, SubLinkType, UpdateStmt,
};
use rupg_types::oid;

use crate::Analyzer;
use crate::agg::Kind;
use crate::coerce::AtOpt;
use crate::expr::{Expr, ExprKind, Var};
use crate::from::{Column, FromItem};
use crate::insert::{Item, Write, WriteKind, not_yet, targets, with_checks};
use crate::select::{Query, Target};
use crate::sort::raw_location;
use crate::typename::place;

/// `SelfItemPointerAttributeNumber`: the attribute number of `ctid`, which reads the ID of the version of a row of the target table.
const CTID_ATTNUM: i16 = -1;

/// The names of the system columns, which `SET` cannot assign.
const SYSTEM_COLUMNS: [&str; 6] = ["tableoid", "ctid", "xmin", "cmin", "xmax", "cmax"];

impl Analyzer<'_> {
    /// `transformUpdateStmt`: the query tree of an `UPDATE`.
    pub(crate) fn update(&mut self, s: &UpdateStmt) -> Result<Query> {
        self.write_with(s.withClause.as_deref())?;
        let rv = s.relation.as_deref().ok_or_else(|| Error::internal("an UPDATE with no table"))?;
        let (table, name, columns) = self.target_table(rv, WriteKind::Update)?;
        let from = self.target_from(rv, &s.fromClause)?;
        let filter = self.target_where(s.whereClause.as_ref())?;
        let row = self.update_row(rv, table, &name, &columns, &s.targetList)?;
        let (returning, carried) =
            self.write_returning(s.returningClause.as_deref(), WriteKind::Update)?;
        let tail = tail_columns(&carried);
        let mut query = Query {
            relations: std::mem::take(&mut self.scope.relations),
            from,
            targets: targets(&columns, row),
            filter,
            ..Query::default()
        };
        query.targets.extend(carried_targets(carried));
        query.targets.push(tid_target());
        let checks = self.checks(table)?;
        let names = checks.iter().map(|(n, _)| n.clone()).collect();
        if !checks.is_empty() {
            query = with_checks(&name, &columns, query, checks, &tail);
        }
        let width = columns.len();
        let unique = self.unique_keys(table, &columns)?;
        let foreign = self.foreign_keys(table, WriteKind::Update, &columns)?;
        query.write = Some(Write {
            kind: WriteKind::Update,
            table,
            name,
            width,
            checks: names,
            unique,
            returning,
            foreign,
            on_conflict: None,
            merge: None,
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
        let mut given: Vec<(usize, &ResTarget, Option<Expr>)> = Vec::with_capacity(items.len());
        for (rt, item) in items {
            let col_name = rt.name.as_deref().unwrap_or_default();
            let at = place(rt.location);
            let Some(index) = columns.iter().position(|c| c.name == col_name) else {
                if SYSTEM_COLUMNS.contains(&col_name) {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!("cannot assign to system column \"{col_name}\""),
                    )
                    .at_opt(at));
                }
                let mut error = Error::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!("column \"{col_name}\" of relation \"{name}\" does not exist"),
                );
                let alias = rv.alias.as_ref().and_then(|a| a.aliasname.as_deref());
                if !rt.indirection.is_empty() && alias.or(rv.relname.as_deref()) == Some(col_name) {
                    error = error.with_hint(
                        "SET target columns cannot be qualified with the relation name.",
                    );
                }
                return Err(error.at_opt(at));
            };
            if !rt.indirection.is_empty() {
                return Err(not_yet("assignment to a field or an element in UPDATE").at_opt(at));
            }
            let expr = match item {
                Item::Default(_) => None,
                Item::Expr(expr) => Some(self.assign(expr, &columns[index])?),
            };
            given.push((index, rt, expr));
        }
        // `process_matched_tle` of `rewriteTargetListIU`: a column gets one item.
        for (i, &(index, _, _)) in given.iter().enumerate() {
            if given[..i].iter().any(|&(other, _, _)| other == index) {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("multiple assignments to same column \"{}\"", columns[index].name),
                ));
            }
        }
        let mut slots: Vec<Option<Option<Expr>>> = vec![None; columns.len()];
        for (index, _, expr) in given {
            slots[index] = Some(expr);
        }
        let mut row = Vec::with_capacity(columns.len());
        for (index, slot) in slots.into_iter().enumerate() {
            let column = &columns[index];
            row.push(match slot {
                Some(Some(expr)) => expr,
                Some(None) => self.column_default(table, index, column)?,
                None => {
                    let attnum = i16::try_from(index + 1).unwrap_or(i16::MAX);
                    let mut expr = Expr::new(
                        ExprKind::Var(Var { relation: 0, attnum, levels_up: 0 }),
                        column.ty,
                    );
                    expr.typmod = column.typmod;
                    expr
                }
            });
        }
        Ok(row)
    }

    /// `transformDeleteStmt`: the query tree of a `DELETE`.
    pub(crate) fn delete(&mut self, s: &DeleteStmt) -> Result<Query> {
        self.write_with(s.withClause.as_deref())?;
        let rv = s.relation.as_deref().ok_or_else(|| Error::internal("a DELETE with no table"))?;
        let (table, name, columns) = self.target_table(rv, WriteKind::Delete)?;
        let foreign = self.foreign_keys(table, WriteKind::Delete, &columns)?;
        let from = self.target_from(rv, &s.usingClause)?;
        let filter = self.target_where(s.whereClause.as_ref())?;
        let (returning, carried) =
            self.write_returning(s.returningClause.as_deref(), WriteKind::Delete)?;
        // With `RETURNING`, each row gives the old row and the carried values before the ID of its version.
        let mut targets = Vec::new();
        if returning.is_some() {
            targets = old_row(&columns);
            targets.extend(carried_targets(carried));
        }
        targets.push(tid_target());
        let mut query = Query {
            relations: std::mem::take(&mut self.scope.relations),
            from,
            targets,
            filter,
            write: Some(Write {
                kind: WriteKind::Delete,
                table,
                name,
                width: 0,
                checks: Vec::new(),
                unique: Vec::new(),
                returning,
                foreign,
                on_conflict: None,
                merge: None,
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

    /// `transformWhereClause` of the statement, with `WHERE CURRENT OF` as an error.
    pub(crate) fn target_where(&mut self, node: Option<&Node>) -> Result<Option<Expr>> {
        match node {
            None => Ok(None),
            Some(Node::CurrentOfExpr(_)) => Err(not_yet("WHERE CURRENT OF")),
            Some(node) => {
                let expr = self.with_kind(Kind::Where, |a| a.transform(Some(node)))?;
                Ok(Some(self.coerce_to_boolean(expr, "WHERE")?))
            }
        }
    }

    /// `transformTargetList` for the items of `SET`, with `DEFAULT` kept at the top of an item. An item of `SET (a, b) = ROW(...)` takes its field of the row.
    fn set_items<'s>(
        &mut self,
        list: &'s rupg_sql::nodes::List,
    ) -> Result<Vec<(&'s ResTarget, Item)>> {
        let mut items = Vec::with_capacity(list.len());
        let mut fields: Vec<Option<Item>> = Vec::new();
        for node in list {
            let Some(Node::ResTarget(rt)) = node else {
                return Err(Error::internal("a SET item that is not ResTarget"));
            };
            let item = match &rt.val {
                Some(Node::MultiAssignRef(multi)) => {
                    if multi.colno == 1 {
                        fields = self.multi_source(multi.source.as_ref(), multi.ncolumns)?;
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

    /// `transformMultiAssignRef`: the fields of the source of `SET (a, b) = ...`. A `ROW` expression gives its fields, which can be `DEFAULT`.
    fn multi_source(&mut self, source: Option<&Node>, ncolumns: i32) -> Result<Vec<Option<Item>>> {
        let at = raw_location(source);
        match source {
            Some(Node::SubLink(link)) if link.subLinkType == SubLinkType::EXPR_SUBLINK => {
                Err(not_yet("a sub-SELECT as the source of a multiple-column UPDATE item")
                    .at_opt(at))
            }
            Some(Node::RowExpr(row)) => {
                if usize::try_from(ncolumns).ok() != Some(row.args.len()) {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        "number of columns does not match number of values",
                    )
                    .at_opt(at));
                }
                let mut fields = Vec::with_capacity(row.args.len());
                for arg in &row.args {
                    fields.push(Some(match arg {
                        Some(Node::SetToDefault(d)) => Item::Default(place(d.location)),
                        arg => Item::Expr(
                            self.with_kind(Kind::UpdateSource, |a| a.transform(arg.as_ref()))?,
                        ),
                    }));
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
