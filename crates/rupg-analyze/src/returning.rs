//! `transformReturningClause` and `addNSItemForReturning` of `analyze.c`: the list of `RETURNING` of an `INSERT`, an `UPDATE` or a `DELETE`, with the names `old` and `new` of the old and the new row.
//!
//! The analysis gives a query that reads one relation: the rows that the statement writes. A row of it has the columns of the table, then the carried values. For an `INSERT` and an `UPDATE` the columns are the new row, and for a `DELETE` they are the old row. A carried value is a value that the query of the statement gives for each row after the columns: a column of a relation of `FROM` or `USING`, or a column of the old row of an `UPDATE`. A column of a row that the statement does not have, as the old row of an `INSERT`, is null.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{Node, ReturningClause, ReturningOptionKind};
use rupg_types::{Value, oid};

use crate::Analyzer;
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::expr::{Expr, ExprKind, Var};
use crate::from::{FromItem, Relation};
use crate::insert::WriteKind;
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
        let result = self.returning_list(clause, kind);
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
        let source = |relation: usize| {
            if Some(relation) == old {
                match kind {
                    WriteKind::Insert => Source::Null,
                    WriteKind::Update => Source::Carry(0),
                    WriteKind::Delete => Source::Row,
                }
            } else if Some(relation) == new {
                match kind {
                    WriteKind::Delete => Source::Null,
                    WriteKind::Insert | WriteKind::Update => Source::Row,
                }
            } else if relation == 0 {
                Source::Row
            } else {
                Source::Carry(relation)
            }
        };
        let table = self
            .scope
            .relations
            .first()
            .ok_or_else(|| Error::internal("RETURNING with no table"))?;
        let width = table.columns.len();
        let mut carried: Vec<Expr> = Vec::new();
        let mut change = |expr: &mut Expr, depth: usize| {
            let ExprKind::Var(var) = expr.kind else { return };
            if var.levels_up != depth {
                return;
            }
            match source(var.relation) {
                Source::Row => expr.kind = ExprKind::Var(Var { relation: 0, ..var }),
                Source::Null => expr.kind = ExprKind::Const(Value::Null),
                Source::Carry(relation) => {
                    let found = carried.iter().position(|c| {
                        matches!(c.kind, ExprKind::Var(v) if v.relation == relation && v.attnum == var.attnum)
                    });
                    let at = found.unwrap_or_else(|| {
                        let mut value = Expr::new(
                            ExprKind::Var(Var { relation, attnum: var.attnum, levels_up: 0 }),
                            expr.ty,
                        );
                        value.typmod = expr.typmod;
                        carried.push(value);
                        carried.len() - 1
                    });
                    let attnum = i16::try_from(width + 1 + at).unwrap_or(i16::MAX);
                    expr.kind = ExprKind::Var(Var { relation: 0, attnum, levels_up: depth });
                }
            }
        };
        for target in &mut targets {
            rewrite(&mut target.expr, 0, &mut change);
        }
        let mut relation = Relation::new(table.oid, table.columns.clone(), None, None, None);
        relation.name.clone_from(&table.name);
        relation.alias.clone_from(&table.alias);
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
fn rewrite(expr: &mut Expr, depth: usize, f: &mut dyn FnMut(&mut Expr, usize)) {
    f(expr, depth);
    for child in expr.children_mut() {
        rewrite(child, depth, f);
    }
    if let ExprKind::SubLink(sub) = &mut expr.kind {
        sub.query.each_expr_mut(depth + 1, &mut |e, depth| rewrite(e, depth, &mut *f));
    }
}
