//! `transformInsertStmt` of `analyze.c`, with `checkInsertTargets` and `transformAssignedExpr` of `parse_target.c` and the defaults of `rewriteTargetListIU` and `expand_insert_targetlist`.
//!
//! The query of an `INSERT` gives the full row of the table for each new row, in the order of the attributes. A column that the statement does not give gets its default, the default of its domain or a null value. When the table has check constraints, the query reads the rows as a subquery and its targets after the columns are the conditions of the constraints in the order of their names, which the executor tests.

use rupg_catalog::{ConKind, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{InsertStmt, Node, SelectStmt};
use rupg_types::{Value, oid};

use crate::Analyzer;
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::expr::{Expr, ExprKind, Var};
use crate::from::{Column, FromItem, Relation};
use crate::select::{Query, Target};
use crate::typename::place;
use crate::types;

/// The kind of a statement that writes the rows of a table, `CmdType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteKind {
    Insert,
    Update,
    Delete,
}

impl WriteKind {
    /// The name of the statement.
    pub fn name(self) -> &'static str {
        match self {
            WriteKind::Insert => "INSERT",
            WriteKind::Update => "UPDATE",
            WriteKind::Delete => "DELETE",
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
}

/// A unique index of the target table, which `_bt_check_unique` checks for each new version of a row.
#[derive(Clone, Debug, PartialEq)]
pub struct Unique {
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

/// A column that the statement gives: the index of the column in the table and the place of its name in the column list.
type Given = (usize, Option<usize>);

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
        if s.withClause.is_some() {
            return Err(not_yet("WITH in INSERT"));
        }
        if s.onConflictClause.is_some() {
            return Err(not_yet("ON CONFLICT"));
        }
        if s.returningClause.is_some() {
            return Err(not_yet("RETURNING"));
        }
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
                self.insert_select(select, table, &columns, &given, explicit)?
            }
            Some(select) if select.valuesLists.len() > 1 => {
                self.insert_values(select, table, &columns, &given, explicit)?
            }
            Some(select) => {
                let Some(Some(Node::List(list))) = select.valuesLists.first() else {
                    return Err(Error::internal("a VALUES row that is not a list"));
                };
                let items = self.value_items(list, Kind::ValuesSingle)?;
                let exprs = self.insert_row(items, table, &columns, &given, explicit)?;
                let row = self.full_row(table, &columns, &given, exprs)?;
                row_query(&columns, row)
            }
        };
        let checks = self.checks(table)?;
        let names = checks.iter().map(|(n, _)| n.clone()).collect();
        if !checks.is_empty() {
            query = with_checks(&name, &columns, query, checks, false);
        }
        let width = columns.len();
        let unique = self.unique_keys(table, &columns)?;
        query.write =
            Some(Write { kind: WriteKind::Insert, table, name, width, checks: names, unique });
        Ok(query)
    }

    /// `setTargetTable`: the OID, the name and the columns of the table that the statement writes. A table with a constraint or an object that the executor does not check yet gives `0A000`.
    pub(crate) fn target_table(
        &self,
        rv: &rupg_sql::nodes::RangeVar,
        kind: WriteKind,
    ) -> Result<(u32, String, Vec<Column>)> {
        let what = match kind {
            WriteKind::Insert => "INSERT into",
            WriteKind::Update => "UPDATE of",
            WriteKind::Delete => "DELETE from",
        };
        let at = place(rv.location);
        let name = rv.relname.as_deref().unwrap_or_default().to_string();
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
            RelKind::View => return Err(not_yet(&format!("{what} a view"))),
            _ => {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("cannot change relation \"{name}\""),
                ));
            }
        }
        if let Some(catalog) = self.env.catalog() {
            let foreign = catalog.constraints_of(oid).any(|c| c.kind == ConKind::Foreign);
            if foreign && kind != WriteKind::Delete {
                return Err(not_yet(&format!("{what} a table with a foreign key")));
            }
            let referenced =
                catalog.constraints().any(|c| c.foreign.as_ref().is_some_and(|f| f.table == oid));
            if referenced && kind != WriteKind::Insert {
                return Err(not_yet(&format!("{what} a table that a foreign key references")));
            }
            if rel.has_triggers || catalog.triggers().any(|t| t.relation == oid) {
                return Err(not_yet(&format!("{what} a table with triggers")));
            }
            if catalog.rules().any(|r| r.relation == oid) {
                return Err(not_yet(&format!("{what} a table with rules")));
            }
        }
        let columns = rel
            .columns
            .iter()
            .map(|c| Column {
                name: c.name.clone(),
                ty: c.ty,
                typmod: c.typmod,
                not_null: c.not_null,
            })
            .collect();
        Ok((oid, rel.name.clone(), columns))
    }

    /// The unique indexes of a table in the order of their OIDs. A unique index with an expression, a predicate or a deferrable constraint is not supported yet.
    pub(crate) fn unique_keys(&self, table: u32, columns: &[Column]) -> Result<Vec<Unique>> {
        let Some(catalog) = self.env.catalog() else {
            return Ok(Vec::new());
        };
        let mut indexes: Vec<_> = catalog
            .relations()
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
    fn insert_targets(
        &self,
        cols: &rupg_sql::nodes::List,
        name: &str,
        columns: &[Column],
    ) -> Result<Vec<Given>> {
        if cols.is_empty() {
            return Ok((0..columns.len()).map(|i| (i, None)).collect());
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
            if !rt.indirection.is_empty() {
                return Err(not_yet("assignment to a field or an element in INSERT").at_opt(at));
            }
            if given.iter().any(|&(i, _)| i == index) {
                return Err(Error::new(
                    SqlState::DUPLICATE_COLUMN,
                    format!("column \"{col_name}\" specified more than once"),
                )
                .at_opt(at));
            }
            given.push((index, at));
        }
        Ok(given)
    }

    /// `transformExpressionList` for a row of `VALUES` in an `INSERT`, which keeps `DEFAULT` at the top of an item.
    fn value_items(&mut self, list: &rupg_sql::nodes::List, kind: Kind) -> Result<Vec<Item>> {
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

    /// `transformInsertRow`: the items of a row, each with the type of its column, and `DEFAULT` as the default of the column.
    fn insert_row(
        &mut self,
        items: Vec<Item>,
        table: u32,
        columns: &[Column],
        given: &[Given],
        explicit: bool,
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
        for (item, &(index, _)) in items.into_iter().zip(given) {
            row.push(match item {
                Item::Default(_) => self.column_default(table, index, &columns[index])?,
                Item::Expr(expr) => self.assign(expr, &columns[index])?,
            });
        }
        Ok(row)
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

    /// `build_column_default`: the default of the column, or the default of its domain, cast to the type of the column. With no default, the value is a null value of the type, which a domain checks, as `coerce_null_to_domain` gives.
    pub(crate) fn column_default(
        &mut self,
        table: u32,
        index: usize,
        column: &Column,
    ) -> Result<Expr> {
        let attnum = i16::try_from(index + 1).unwrap_or(i16::MAX);
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
    fn full_row(
        &mut self,
        table: u32,
        columns: &[Column],
        given: &[Given],
        exprs: Vec<Expr>,
    ) -> Result<Vec<Expr>> {
        let mut slots: Vec<Option<Expr>> = vec![None; columns.len()];
        for (expr, &(index, _)) in exprs.into_iter().zip(given) {
            slots[index] = Some(expr);
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
        let exprs = self.insert_row(items, table, columns, given, explicit)?;
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
            rows.push(self.insert_row(items, table, columns, given, explicit)?);
        }
        let width = rows.first().map_or(0, Vec::len);
        let value_columns: Vec<Column> = given
            .iter()
            .take(width)
            .enumerate()
            .map(|(i, &(index, _))| Column {
                name: format!("column{}", i + 1),
                ty: columns[index].ty,
                typmod: columns[index].typmod,
                not_null: false,
            })
            .collect();
        let exprs: Vec<Expr> = value_columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
                let mut expr =
                    Expr::new(ExprKind::Var(Var { relation: 0, attnum, levels_up: 0 }), c.ty);
                expr.typmod = c.typmod;
                expr
            })
            .collect();
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

/// The query that reads the rows of `rows` as a subquery with the name of the table, and gives the columns and then the condition of each check constraint. With `tid`, the rows end with the ID of the version of the old row, which comes after the conditions.
pub(crate) fn with_checks(
    name: &str,
    columns: &[Column],
    rows: Query,
    checks: Vec<(String, Expr)>,
    tid: bool,
) -> Query {
    let mut columns = columns.to_vec();
    if tid {
        columns.push(Column {
            name: "ctid".to_string(),
            ty: oid::INT8,
            typmod: -1,
            not_null: false,
        });
    }
    let width = columns.len() - usize::from(tid);
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
    let tid = targets.split_off(width);
    targets.extend(checks.into_iter().map(|(n, expr)| Target {
        name: n,
        expr,
        origin: None,
        junk: false,
    }));
    targets.extend(tid);
    let mut relation = Relation::new(0, columns, Some(Box::new(rows)), None, None);
    relation.name = name.to_string();
    Query {
        relations: vec![relation],
        from: vec![FromItem::Relation(0)],
        targets,
        ..Query::default()
    }
}
