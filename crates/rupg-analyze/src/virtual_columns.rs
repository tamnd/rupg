//! `expand_virtual_generated_columns` of `prepjointree.c`: the planner puts the generation expression of each virtual generated column in the place of each read of the column. The heap keeps a null for the column, so a read of the row computes the column from the other columns.

use std::collections::BTreeMap;
use std::rc::Rc;

use rupg_catalog::{Catalog, RelKind};
use rupg_common::Result;
use rupg_types::Value;

use crate::Body;
use crate::expr::{CastForm, Expr, ExprKind, PlaceHolder, Var};
use crate::from::{FromItem, JoinKind};
use crate::insert::WriteKind;
use crate::merge::MergeCommand;
use crate::select::Query;

/// The columns of a table with a virtual generated column.
struct Table {
    /// For each column, the type, the typmod and `attisdropped`.
    columns: Vec<(u32, i32, bool)>,
    /// For each column, the generation expression of a virtual generated column, which reads the relation 0.
    exprs: Vec<Option<Expr>>,
}

/// The relations of one query of the tree.
struct Level {
    /// For each relation, the table with a virtual generated column that it reads.
    tables: Vec<Option<Rc<Table>>>,
    /// For each relation, true when an outer join can make its columns null.
    nulled: Vec<bool>,
    /// For each relation, the number of rows of the table in each of its rows. A row of the query of `RETURNING` of a `MERGE` has the old row and the new row after the row.
    copies: Vec<usize>,
}

/// The state of the expansion.
struct Expand<'a> {
    catalog: &'a Catalog,
    /// The tables that the expansion read, by OID, or `None` for a relation with no virtual generated column.
    tables: BTreeMap<u32, Option<Rc<Table>>>,
    /// The levels of the queries around the expression, the inner query last.
    levels: Vec<Level>,
    /// The number of rows of the table in each row of the relation 0 of the next query, for the query of `RETURNING`.
    copies: usize,
}

/// `expand_virtual_generated_columns`: the query with the generation expression of each virtual generated column in the place of each column of the table that the query reads, also in its subqueries and in the queries of its write. The rows of a write keep the null of each virtual generated column of the table.
///
/// # Errors
///
/// The error of a generation expression that the catalog keeps in a form that the analyzer cannot read.
pub fn expand_virtual(query: &mut Query, catalog: &Catalog) -> Result<()> {
    Expand::new(catalog).query(query, None)
}

/// [`expand_virtual`] for the body of a function in SQL.
///
/// # Errors
///
/// The error of a generation expression that the catalog keeps in a form that the analyzer cannot read.
pub fn expand_virtual_body(body: &mut Body, catalog: &Catalog) -> Result<()> {
    let mut expand = Expand::new(catalog);
    match body {
        Body::Value(expr) => expand.expr(expr),
        Body::Query(query) => expand.query(query, None),
        // The plan of each statement expands its own columns.
        Body::Statements(_) => Ok(()),
    }
}

/// Marks the relations that an outer join of `item` can make null. With `nullable`, a join above `item` can make all its relations null.
fn mark(item: &FromItem, nullable: bool, nulled: &mut [bool]) {
    match item {
        FromItem::Relation(index) => {
            if nullable && let Some(slot) = nulled.get_mut(*index) {
                *slot = true;
            }
        }
        FromItem::Join(join) => {
            let (left, right) = match join.kind {
                JoinKind::Inner | JoinKind::Semi | JoinKind::RightSemi => (false, false),
                JoinKind::Left | JoinKind::Anti => (false, true),
                JoinKind::Right | JoinKind::RightAnti => (true, false),
                JoinKind::Full => (true, true),
            };
            mark(&join.left, nullable || left, nulled);
            mark(&join.right, nullable || right, nulled);
        }
    }
}

/// The expressions of the joins of `item`: the conditions, the merged columns of `USING` and the columns of the joins.
fn join_exprs<'q>(item: &'q mut FromItem, out: &mut Vec<&'q mut Expr>) {
    if let FromItem::Join(join) = item {
        let join = &mut **join;
        join_exprs(&mut join.left, out);
        join_exprs(&mut join.right, out);
        out.extend(join.on.as_mut());
        out.extend(join.merged.iter_mut());
        out.extend(join.columns.iter_mut().map(|(_, e)| e));
    }
}

/// The generation expression `expr`, which reads the relation 0, for a column of the relation `relation` that is `levels_up` levels up, with the columns after `shift` columns of the row.
fn rebase(mut expr: Expr, relation: usize, levels_up: usize, shift: usize) -> Expr {
    fn walk(expr: &mut Expr, relation: usize, levels_up: usize, shift: i16) {
        if let ExprKind::Var(var) = &mut expr.kind
            && var.levels_up == 0
            && var.relation == 0
        {
            var.relation = relation;
            var.levels_up = levels_up;
            if var.attnum > 0 {
                var.attnum = var.attnum.saturating_add(shift);
            }
        }
        for child in expr.children_mut() {
            walk(child, relation, levels_up, shift);
        }
    }
    walk(&mut expr, relation, levels_up, i16::try_from(shift).unwrap_or(i16::MAX));
    expr
}

impl<'a> Expand<'a> {
    /// The state of an expansion with no level.
    fn new(catalog: &'a Catalog) -> Self {
        Expand { catalog, tables: BTreeMap::new(), levels: Vec::new(), copies: 1 }
    }

    /// The table with a virtual generated column that a relation with the OID `oid` reads, or `None`.
    fn table(&mut self, oid: u32) -> Result<Option<Rc<Table>>> {
        if let Some(table) = self.tables.get(&oid) {
            return Ok(table.clone());
        }
        let found = match self.catalog.relation(oid) {
            Some(rel)
                if matches!(rel.kind, RelKind::Table | RelKind::Partitioned)
                    && rel.columns.iter().any(|c| c.generated == b'v' && !c.dropped) =>
            {
                let mut exprs = Vec::with_capacity(rel.columns.len());
                for (index, column) in rel.columns.iter().enumerate() {
                    let attnum = i16::try_from(index + 1).unwrap_or(i16::MAX);
                    let expr = match self.catalog.default_of(oid, attnum) {
                        Some(default) if column.generated == b'v' && !column.dropped => {
                            Some(crate::node::read(&default.expr)?)
                        }
                        _ => None,
                    };
                    exprs.push(expr);
                }
                let columns = rel.columns.iter().map(|c| (c.ty, c.typmod, c.dropped)).collect();
                Some(Rc::new(Table { columns, exprs }))
            }
            _ => None,
        };
        self.tables.insert(oid, found.clone());
        Ok(found)
    }

    /// The expansion of a query. `table` is the OID of the table of the relation 0 when the relation reads the rows that a write gives, which have no OID of a table.
    fn query(&mut self, query: &mut Query, table: Option<u32>) -> Result<()> {
        let mut tables = Vec::with_capacity(query.relations.len());
        for (index, relation) in query.relations.iter().enumerate() {
            let oid = match table {
                Some(table) if index == 0 => table,
                _ if relation.subquery.is_some()
                    || relation.function.is_some()
                    || relation.values.is_some()
                    || relation.cte.is_some() =>
                {
                    0
                }
                _ => relation.oid,
            };
            tables.push(if oid == 0 { None } else { self.table(oid)? });
        }
        let mut nulled = vec![false; query.relations.len()];
        for item in &query.from {
            mark(item, false, &mut nulled);
        }
        let mut copies = vec![1; query.relations.len()];
        if let Some(first) = copies.first_mut() {
            *first = std::mem::replace(&mut self.copies, 1);
        }
        let kept = self.kept(query);
        self.levels.push(Level { tables, nulled, copies });
        let result = self.query_exprs(query, &kept);
        self.levels.pop();
        result?;
        // The queries of the write read the rows that the write gives, with no level around them.
        let Some(write) = query.write.as_mut() else { return Ok(()) };
        let levels = std::mem::take(&mut self.levels);
        let result = self.write(write);
        self.levels = levels;
        result
    }

    /// The targets of the query that keep the null of a virtual generated column of the table of its write: the columns of a new row, and the old row of a `MERGE`.
    fn kept(&mut self, query: &Query) -> Vec<bool> {
        let Some(write) = &query.write else { return Vec::new() };
        if write.kind == WriteKind::Delete {
            return Vec::new();
        }
        let Ok(Some(table)) = self.table(write.table) else { return Vec::new() };
        let width = write.width.min(table.exprs.len());
        table.exprs[..width].iter().map(Option::is_some).collect()
    }

    /// The expressions of a query, then its subqueries in `FROM` and in `WITH`.
    fn query_exprs(&mut self, query: &mut Query, kept: &[bool]) -> Result<()> {
        let mut exprs = Vec::new();
        for (index, target) in query.targets.iter_mut().enumerate() {
            if kept.get(index).copied().unwrap_or(false) {
                let mut null = Expr::new(ExprKind::Const(Value::Null), target.expr.ty);
                null.typmod = target.expr.typmod;
                target.expr = null;
            } else {
                exprs.push(&mut target.expr);
            }
        }
        for item in &mut query.from {
            join_exprs(item, &mut exprs);
        }
        exprs.extend(
            query
                .filter
                .iter_mut()
                .chain(&mut query.having)
                .chain(&mut query.offset)
                .chain(&mut query.limit),
        );
        for window in &mut query.windows {
            exprs.extend(window.frame.start.iter_mut().chain(&mut window.frame.end));
        }
        for relation in &mut query.relations {
            if let Some(function) = &mut relation.function {
                exprs.extend(function.calls.iter_mut().map(|(call, _)| call));
            }
            exprs.extend(relation.values.iter_mut().flatten().flatten());
            if let Some(sample) = &mut relation.tablesample {
                exprs.extend(sample.args.iter_mut().chain(&mut sample.repeatable));
            }
        }
        for expr in exprs {
            self.expr(expr)?;
        }
        for relation in &mut query.relations {
            if let Some(sub) = &mut relation.subquery {
                self.query(sub, None)?;
            }
        }
        for cte in &mut query.ctes {
            self.query(&mut cte.query, None)?;
        }
        Ok(())
    }

    /// The expansion of the queries of a write.
    fn write(&mut self, write: &mut crate::Write) -> Result<()> {
        if let Some(returning) = &mut write.returning {
            self.returning(returning, write.kind)?;
        }
        if let Some(check) = &mut write.view_check {
            self.query(&mut check.query, Some(write.table))?;
        }
        if let Some(recheck) = &mut write.recheck {
            self.query(recheck, None)?;
        }
        let kept = match self.table(write.table)? {
            Some(table) => table.exprs.iter().map(Option::is_some).collect(),
            None => Vec::new(),
        };
        if let Some(update) = write.on_conflict.as_mut().and_then(|c| c.update.as_mut()) {
            self.new_row(update, &kept)?;
        }
        for clause in write.merge.iter_mut().flat_map(|m| &mut m.clauses) {
            if matches!(clause.command, MergeCommand::Insert | MergeCommand::Update) {
                self.new_row(&mut clause.query, &kept)?;
            } else {
                self.query(&mut clause.query, None)?;
            }
        }
        Ok(())
    }

    /// The expansion of a query that gives a new row of the table, whose virtual generated columns `kept` keep their nulls.
    fn new_row(&mut self, query: &mut Query, kept: &[bool]) -> Result<()> {
        for (target, kept) in query.targets.iter_mut().zip(kept) {
            if *kept {
                let mut null = Expr::new(ExprKind::Const(Value::Null), target.expr.ty);
                null.typmod = target.expr.typmod;
                target.expr = null;
            }
        }
        self.query(query, None)
    }

    /// The expansion of the query of `RETURNING`, whose relation 0 is the rows of the write. A row of a `MERGE` has the old row and the new row after the columns.
    fn returning(&mut self, query: &mut Query, kind: WriteKind) -> Result<()> {
        self.copies = if kind == WriteKind::Merge { 3 } else { 1 };
        self.query(query, None)
    }

    /// The expansion of an expression and of the subqueries in it.
    fn expr(&mut self, expr: &mut Expr) -> Result<()> {
        if let ExprKind::Var(var) = expr.kind {
            if let Some(new) = self.var(expr, var) {
                *expr = new;
            }
            return Ok(());
        }
        for child in expr.children_mut() {
            self.expr(child)?;
        }
        if let ExprKind::SubLink(sub) = &mut expr.kind {
            self.query(&mut sub.query, None)?;
        }
        Ok(())
    }

    /// The generation expression in the place of the column `var`, or `None` for a column that is not a virtual generated column. A column that an outer join can make null becomes a `PlaceHolderVar`, which is null for a null row of the join.
    fn var(&self, expr: &Expr, var: Var) -> Option<Expr> {
        let level = self.levels.len().checked_sub(var.levels_up + 1)?;
        let level = self.levels.get(level)?;
        let table = level.tables.get(var.relation)?.as_ref()?;
        let new = if var.attnum == 0 {
            // `ReplaceVarsFromTargetList`: the whole row is a `RowExpr` of the columns, with no dropped column.
            let fields = table
                .columns
                .iter()
                .zip(&table.exprs)
                .enumerate()
                .filter(|(_, ((_, _, dropped), _))| !dropped)
                .map(|(i, ((ty, typmod, _), gen_expr))| match gen_expr {
                    Some(e) => rebase(e.clone(), var.relation, var.levels_up, 0),
                    None => {
                        let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
                        let column =
                            Var { relation: var.relation, attnum, levels_up: var.levels_up };
                        let mut field = Expr::new(ExprKind::Var(column), *ty);
                        field.typmod = *typmod;
                        field
                    }
                })
                .collect();
            Expr { kind: ExprKind::Row(fields, CastForm::Implicit), ..expr.clone() }
        } else {
            let width = table.exprs.len();
            let at = usize::try_from(var.attnum).ok()?.checked_sub(1)?;
            if at / width >= level.copies.get(var.relation).copied().unwrap_or(1) {
                return None;
            }
            let shift = at / width * width;
            let gen_expr = table.exprs.get(at % width)?.as_ref()?;
            let mut new = rebase(gen_expr.clone(), var.relation, var.levels_up, shift);
            new.location = expr.location;
            new
        };
        if !level.nulled.get(var.relation).copied().unwrap_or(false) {
            return Some(new);
        }
        let (ty, typmod, location) = (new.ty, new.typmod, new.location);
        let p = PlaceHolder {
            expr: new,
            relations: vec![var.relation],
            levels_up: var.levels_up,
            removed: false,
        };
        Some(Expr { kind: ExprKind::PlaceHolder(Box::new(p)), ty, typmod, location })
    }
}
