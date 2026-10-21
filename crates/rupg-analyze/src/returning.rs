//! `transformReturningClause` and `addNSItemForReturning` of `analyze.c`: the list of `RETURNING` of an `INSERT`, an `UPDATE` or a `DELETE`, with the names `old` and `new` of the old and the new row.
//!
//! The analysis gives a query that reads one relation: the rows that the statement writes. A row of it has the columns of the table, then the carried values. For an `INSERT` and an `UPDATE` the columns are the new row, and for a `DELETE` they are the old row. A carried value is a value that the query of the statement gives for each row after the columns: a column of a relation of `FROM` or `USING`, or a column of the old row of an `UPDATE`. A column of a row that the statement does not have, as the old row of an `INSERT`, is null.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{Node, ReturningClause, ReturningOptionKind};
use rupg_types::{Value, oid};

use crate::Analyzer;
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::expr::{Case, CastForm, Expr, ExprKind, Var};
use crate::from::{Column, FromItem, Relation, TABLE_OID_ATTNUM};
use crate::insert::WriteKind;
use crate::merge::carry;
use crate::select::Query;
use crate::typename::place;

/// The place where a column of `RETURNING` takes its value.
enum Source {
    /// The row that the statement writes, which is the relation of the query of `RETURNING`.
    Row,
    /// A value that the query of the statement carries for each row: the column of this relation of the statement.
    Carry(usize),
    /// A row that the statement does not have.
    Null,
    /// The columns of the row of `RETURNING` of a `MERGE` after this number of columns: the old row, the new row or the action.
    Shift(usize),
}

impl Analyzer<'_> {
    /// `transformReturningClause`: the query of `RETURNING` and the values that the query of the statement must carry for it. The first relation of the scope is the target table, and the other relations are the relations of `FROM` or `USING`. The names `old` and `new` stay in the scope only while the analysis reads the list.
    ///
    /// # Errors
    ///
    /// `42601` for an option that the clause gives twice or for a list with no column, `42712` for a name of an option that the scope has, and the errors of the expressions.
    pub(crate) fn returning(
        &mut self,
        clause: &ReturningClause,
        kind: WriteKind,
    ) -> Result<(Query, Vec<Expr>)> {
        let size = self.scope_size();
        // `merge_action()` reads the relation of the action, which no name can find.
        if kind == WriteKind::Merge {
            let column = Column {
                name: "merge_action".to_string(),
                ty: oid::TEXT,
                typmod: -1,
                not_null: false,
            };
            self.scope.merge_action = Some(self.scope.relations.len());
            self.scope.relations.push(Relation::new(0, vec![column], None, None, None));
        }
        let result = self.returning_list(clause, kind);
        self.scope.merge_action = None;
        self.truncate_scope(size);
        result
    }

    /// The body of [`Analyzer::returning`], which adds the names `old` and `new` to the scope.
    fn returning_list(
        &mut self,
        clause: &ReturningClause,
        kind: WriteKind,
    ) -> Result<(Query, Vec<Expr>)> {
        let mut names: [Option<(String, Option<usize>)>; 2] = [None, None];
        for node in &clause.options {
            let Some(Node::ReturningOption(option)) = node else {
                return Err(Error::internal("a RETURNING option that is not ReturningOption"));
            };
            let (slot, what) = if option.option == ReturningOptionKind::RETURNING_OPTION_OLD {
                (0, "OLD")
            } else {
                (1, "NEW")
            };
            let at = place(option.location);
            if names[slot].is_some() {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("{what} cannot be specified multiple times"),
                )
                .at_opt(at));
            }
            names[slot] = Some((option.value.as_deref().unwrap_or_default().to_string(), at));
        }
        // The names `old` and `new` are the default names, unless the scope has a relation with the name.
        for (slot, name) in names.iter_mut().zip(["old", "new"]) {
            if slot.is_none() && !self.has_refname(name)? {
                *slot = Some((name.to_string(), None));
            }
        }
        let mut relations = [None, None];
        for (index, slot) in relations.iter_mut().zip(&names) {
            let Some((name, at)) = slot else { continue };
            if self.has_refname(name)? {
                return Err(Error::new(
                    SqlState::DUPLICATE_ALIAS,
                    format!("table name \"{name}\" specified more than once"),
                )
                .at_opt(*at));
            }
            *index = Some(self.add_returning_item(name)?);
        }
        let mut targets = self.target_list(&clause.exprs, Kind::Returning)?;
        if targets.is_empty() {
            let at = match clause.exprs.first() {
                Some(Some(Node::ResTarget(rt))) => place(rt.location),
                _ => None,
            };
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "RETURNING must have at least one column",
            )
            .at_opt(at));
        }
        self.mark_origins(&mut targets);
        // `resolveTargetListUnknowns`: a column of type `unknown` becomes `text`.
        for target in &mut targets {
            if target.expr.ty == oid::UNKNOWN {
                let expr = std::mem::replace(&mut target.expr, Expr::new(ExprKind::CaseTest, 0));
                target.expr = self.coerce(expr, oid::TEXT, -1, Context::Implicit, None)?;
            }
        }
        let [old, new] = relations;
        let table = self
            .scope
            .relations
            .first()
            .ok_or_else(|| Error::internal("RETURNING with no table"))?;
        let width = table.columns.len();
        let table_oid = table.oid;
        let types: Vec<(u32, i32)> = table.columns.iter().map(|c| (c.ty, c.typmod)).collect();
        let mut relation = Relation::new(table.oid, table.columns.clone(), None, None, None);
        relation.name.clone_from(&table.name);
        relation.alias.clone_from(&table.alias);
        let action = self.scope.merge_action;
        // The whole old row of a `MERGE` is null for an insert, and the whole new row is null for a delete.
        let no_row = if kind == WriteKind::Merge {
            let mut no_row = |name: &str| {
                let attnum = i16::try_from(3 * width + 1).unwrap_or(i16::MAX);
                let var = Var { relation: 0, attnum, levels_up: 0 };
                let action = Expr::new(ExprKind::Var(var), oid::TEXT);
                let name = Expr::new(ExprKind::Const(Value::Text(name.into())), oid::TEXT);
                self.make_op(&["="], Some(action), name, None)
            };
            Some((no_row("INSERT")?, no_row("DELETE")?))
        } else {
            None
        };
        let source = |relation: usize| {
            if Some(relation) == old {
                match kind {
                    WriteKind::Insert => Source::Null,
                    WriteKind::Update => Source::Carry(0),
                    WriteKind::Delete => Source::Row,
                    WriteKind::Merge => Source::Shift(width),
                }
            } else if Some(relation) == new {
                match kind {
                    WriteKind::Delete => Source::Null,
                    WriteKind::Insert | WriteKind::Update => Source::Row,
                    WriteKind::Merge => Source::Shift(2 * width),
                }
            } else if Some(relation) == action {
                Source::Shift(3 * width)
            } else if relation == 0 {
                Source::Row
            } else {
                Source::Carry(relation)
            }
        };
        // The carried values of a `MERGE` come after the row, the old row, the new row and the action. A row of a partitioned table has the OID of its partition after the columns, or after the action for a `MERGE`, and a row of a table with children of `INHERITS` has the OID of its table after the columns.
        let partitioned = self.is_partitioned(table_oid) || self.inherits_target(kind);
        let base = match kind {
            WriteKind::Merge => 3 * width + 1 + usize::from(partitioned),
            _ if partitioned => width + 1,
            _ => width,
        };
        let partition_at = if kind == WriteKind::Merge { 3 * width + 2 } else { width + 1 };
        let partition_attnum = i16::try_from(partition_at).unwrap_or(i16::MAX);
        let mut carried: Vec<Expr> = Vec::new();
        let mut change = |expr: &mut Expr, depth: usize| {
            let ExprKind::Var(var) = expr.kind else { return };
            if var.levels_up != depth {
                return;
            }
            match source(var.relation) {
                Source::Row if partitioned && var.attnum == TABLE_OID_ATTNUM => {
                    expr.kind = ExprKind::Var(Var {
                        relation: 0,
                        attnum: partition_attnum,
                        levels_up: depth,
                    });
                }
                Source::Row => expr.kind = ExprKind::Var(Var { relation: 0, ..var }),
                Source::Null => expr.kind = ExprKind::Const(Value::Null),
                Source::Shift(by) if var.attnum == 0 => {
                    // The planner of PostgreSQL reads the whole row as a `RowExpr` of the columns. A dropped column has no field.
                    let fields = types.iter().enumerate().filter(|(_, (ty, _))| *ty != 0);
                    let fields = fields
                        .map(|(i, &(ty, typmod))| {
                            let attnum = i16::try_from(by + i + 1).unwrap_or(i16::MAX);
                            let var = Var { relation: 0, attnum, levels_up: depth };
                            let mut field = Expr::new(ExprKind::Var(var), ty);
                            field.typmod = typmod;
                            field
                        })
                        .collect();
                    let row = Expr::new(ExprKind::Row(fields, CastForm::Implicit), expr.ty);
                    let Some((insert, delete)) = &no_row else { return };
                    let mut when = if by == width { insert.clone() } else { delete.clone() };
                    rewrite(&mut when, 0, &mut |e: &mut Expr, _| {
                        if let ExprKind::Var(v) = &mut e.kind {
                            v.levels_up = depth;
                        }
                    });
                    let null = Expr::new(ExprKind::Const(Value::Null), expr.ty);
                    let case =
                        Case { arg: None, whens: vec![(when, null)], default: Box::new(row) };
                    expr.kind = ExprKind::Case(case);
                }
                Source::Shift(by) => {
                    let attnum =
                        i16::try_from(by).map_or(i16::MAX, |by| by.saturating_add(var.attnum));
                    expr.kind = ExprKind::Var(Var { relation: 0, attnum, levels_up: depth });
                }
                Source::Carry(relation) => {
                    let at = carry(&mut carried, relation, var.attnum, expr);
                    let attnum = i16::try_from(base + 1 + at).unwrap_or(i16::MAX);
                    expr.kind = ExprKind::Var(Var { relation: 0, attnum, levels_up: depth });
                }
            }
        };
        for target in &mut targets {
            rewrite(&mut target.expr, 0, &mut change);
        }
        let query = Query {
            relations: vec![relation],
            from: vec![FromItem::Relation(0)],
            targets,
            ..Query::default()
        };
        Ok((query, carried))
    }
}

/// Calls `f` for the expression and for each expression in it, also in its subqueries, with the number of subqueries between the expression and this expression. `f` sees an expression before the expressions in it.
pub(crate) fn rewrite(expr: &mut Expr, depth: usize, f: &mut dyn FnMut(&mut Expr, usize)) {
    f(expr, depth);
    for child in expr.children_mut() {
        rewrite(child, depth, f);
    }
    if let ExprKind::SubLink(sub) = &mut expr.kind {
        sub.query.each_expr_mut(depth + 1, &mut |e, depth| rewrite(e, depth, &mut *f));
    }
}
