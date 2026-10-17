//! The `SEARCH` and `CYCLE` clauses of a recursive query of `WITH`: the checks of `analyzeCTE` and the rewrite of `rewriteSearchCycle.c`. The rewrite changes the two terms of the `UNION` so that each row of the query also gives the column of the order, or the mark and the path of a cycle.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{CTECycleClause, CTESearchClause, List, Node};
use rupg_types::{Value, oid};

use crate::Analyzer;
use crate::coerce::AtOpt;
use crate::expr::{Case, CastForm, Expr, ExprKind, Func, FuncForm, Var};
use crate::from::{Column, FromItem, Relation};
use crate::select::{Query, Target};
use crate::setop::SetTree;
use crate::sort::set_group;
use crate::typcache;
use crate::typename::place;
use crate::types;

/// `RECORD_EQ_OP`: the `=` operator of two values of `record`.
const RECORD_EQ_OP: u32 = 2988;
/// `F_ARRAY_CAT`.
const ARRAY_CAT: u32 = 383;
/// `F_INT8INC`.
const INT8INC: u32 = 1219;

/// `CTESearchClause`: `SEARCH DEPTH FIRST` or `SEARCH BREADTH FIRST`.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchClause {
    /// `search_col_list`: the columns that give the order.
    pub columns: Vec<String>,
    /// `search_breadth_first`.
    pub breadth: bool,
    /// `search_seq_column`: the name of the column of the order.
    pub seq: String,
    location: Option<usize>,
}

/// `CTECycleClause`: `CYCLE`, with the columns that find a cycle, the column of the mark with its two values, and the column of the path.
#[derive(Clone, Debug, PartialEq)]
pub struct CycleClause {
    /// `cycle_col_list`.
    pub columns: Vec<String>,
    /// `cycle_mark_column`.
    pub mark: String,
    /// `cycle_mark_value`: the mark of a row that closes a cycle.
    pub value: Expr,
    /// `cycle_mark_default`: the mark of the other rows.
    pub default: Expr,
    /// `cycle_mark_type`.
    pub ty: u32,
    /// `cycle_mark_typmod`.
    pub typmod: i32,
    /// `cycle_path_column`.
    pub path: String,
    /// `cycle_mark_neop`: the `<>` operator of the type of the mark, and its function.
    ne: (u32, u32),
    location: Option<usize>,
}

/// The strings of a list of `String` nodes.
fn strings(list: &List) -> Vec<String> {
    list.iter()
        .filter_map(|n| match n {
            Some(Node::String(s)) => Some(s.to_string()),
            _ => None,
        })
        .collect()
}

/// The clause of the raw tree.
pub(crate) fn search_clause(raw: &CTESearchClause) -> SearchClause {
    SearchClause {
        columns: strings(&raw.search_col_list),
        breadth: raw.search_breadth_first,
        seq: raw.search_seq_column.as_deref().unwrap_or_default().to_string(),
        location: place(raw.location),
    }
}

/// The columns that the clauses add after the columns of the query, as `addRangeTableEntryForCTE` adds them to each reference: the column of the order, of type `record` for `BREADTH` or `record[]` for `DEPTH`, then the mark and the path, of type `record[]`.
pub(crate) fn extra_columns(
    search: Option<&SearchClause>,
    cycle: Option<&CycleClause>,
) -> Vec<Column> {
    let mut columns = Vec::new();
    let column =
        |name: &str, ty, typmod| Column { name: name.to_string(), ty, typmod, not_null: false };
    if let Some(s) = search {
        let ty = if s.breadth { oid::RECORD } else { oid::RECORD_ARRAY };
        columns.push(column(&s.seq, ty, -1));
    }
    if let Some(c) = cycle {
        columns.push(column(&c.mark, c.ty, c.typmod));
        columns.push(column(&c.path, oid::RECORD_ARRAY, -1));
    }
    columns
}

/// A syntax error at the place of a clause.
fn syntax(message: String, at: Option<usize>) -> Error {
    Error::new(SqlState::SYNTAX_ERROR, message).at_opt(at)
}

/// The checks of the columns of a clause: each column must be a column of the query and must not come twice.
fn check_columns(what: &str, list: &[String], names: &[&str], at: Option<usize>) -> Result<()> {
    for (i, name) in list.iter().enumerate() {
        if !names.contains(&name.as_str()) {
            return Err(syntax(
                format!("{what} column \"{name}\" not in WITH query column list"),
                at,
            ));
        }
        if list[..i].contains(name) {
            return Err(Error::new(
                SqlState::DUPLICATE_COLUMN,
                format!("{what} column \"{name}\" specified more than once"),
            )
            .at_opt(at));
        }
    }
    Ok(())
}

/// The checks of `analyzeCTE` after the analysis of the query: the query must be recursive with a `SELECT` on each side of the `UNION`, and the names of the clauses must be good.
pub(crate) fn check(
    columns: &[Column],
    recursive: bool,
    location: Option<usize>,
    search: Option<&SearchClause>,
    cycle: Option<&CycleClause>,
    query: &Query,
) -> Result<()> {
    if !recursive {
        return Err(syntax("WITH query is not recursive".to_string(), location));
    }
    let Some(op) = &query.set_op else {
        return Err(Error::internal("a recursive query with no set operation"));
    };
    if !matches!(op.left, SetTree::Leaf(_)) {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "with a SEARCH or CYCLE clause, the left side of the UNION must be a SELECT",
        ));
    }
    if !matches!(op.right, SetTree::Leaf(_)) {
        return Err(syntax(
            "with a SEARCH or CYCLE clause, the right side of the UNION must be a SELECT"
                .to_string(),
            None,
        ));
    }
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    if let Some(s) = search {
        check_columns("search", &s.columns, &names, s.location)?;
        if names.contains(&s.seq.as_str()) {
            return Err(syntax(
                format!(
                    "search sequence column name \"{}\" already used in WITH query column list",
                    s.seq
                ),
                s.location,
            ));
        }
    }
    if let Some(c) = cycle {
        check_columns("cycle", &c.columns, &names, c.location)?;
        if names.contains(&c.mark.as_str()) {
            return Err(syntax(
                format!(
                    "cycle mark column name \"{}\" already used in WITH query column list",
                    c.mark
                ),
                c.location,
            ));
        }
        if names.contains(&c.path.as_str()) {
            return Err(syntax(
                format!(
                    "cycle path column name \"{}\" already used in WITH query column list",
                    c.path
                ),
                c.location,
            ));
        }
        if c.mark == c.path {
            return Err(syntax(
                "cycle mark column name and cycle path column name are the same".to_string(),
                c.location,
            ));
        }
    }
    if let (Some(s), Some(c)) = (search, cycle) {
        if s.seq == c.mark {
            return Err(syntax(
                "search sequence column name and cycle mark column name are the same".to_string(),
                s.location,
            ));
        }
        if s.seq == c.path {
            return Err(syntax(
                "search sequence column name and cycle path column name are the same".to_string(),
                s.location,
            ));
        }
    }
    Ok(())
}

/// A column of the relation with this index.
fn var(relation: usize, index: usize, ty: u32, typmod: i32) -> Expr {
    let attnum = i16::try_from(index + 1).unwrap_or(i16::MAX);
    let var = Var { relation, attnum, levels_up: 0 };
    Expr { kind: ExprKind::Var(var), ty, typmod, location: None }
}

/// A call of the function with this OID.
fn call(func: u32, args: Vec<Expr>, ty: u32) -> Expr {
    let func = Func { oid: func, args, form: FuncForm::Call, variadic: false, retset: false };
    Expr::new(ExprKind::Func(func), ty)
}

/// `make_path_rowexpr`: `ROW(cols)` of the columns of the list, from the first relation.
fn row_of(list: &[String], columns: &[Column]) -> Vec<Expr> {
    list.iter()
        .filter_map(|name| {
            let i = columns.iter().position(|c| &c.name == name)?;
            Some(var(0, i, columns[i].ty, columns[i].typmod))
        })
        .collect()
}

/// `ROW(...)` of type `record`.
fn row(args: Vec<Expr>) -> Expr {
    Expr::new(ExprKind::Row(args, CastForm::Implicit), oid::RECORD)
}

/// `make_path_initial_array`: `ARRAY[ROW(cols)]`.
fn array(args: Vec<Expr>) -> Expr {
    let elements = vec![row(args)];
    Expr::new(
        ExprKind::Array { element: oid::RECORD, multidims: false, elements },
        oid::RECORD_ARRAY,
    )
}

/// `make_path_cat_expr`: `path || ARRAY[ROW(cols)]`, where `path` is the column with this index.
fn path_cat(args: Vec<Expr>, index: usize) -> Expr {
    let path = var(0, index, oid::RECORD_ARRAY, -1);
    call(ARRAY_CAT, vec![path, array(args)], oid::RECORD_ARRAY)
}

/// A query that reads one subquery in `FROM`, as `newq1` and `newq2` of `rewriteSearchAndCycle`.
fn wrap(name: &str, columns: Vec<Column>, sub: Query, targets: Vec<Target>) -> Query {
    let mut relation = Relation::new(0, columns, Some(Box::new(sub)), None, None);
    relation.name = name.to_string();
    Query {
        relations: vec![relation],
        from: vec![FromItem::Relation(0)],
        targets,
        ..Query::default()
    }
}

/// A target that the result shows.
fn target(name: &str, expr: Expr) -> Target {
    Target { name: name.to_string(), expr, origin: None, junk: false }
}

/// `IncrementVarSublevelsUp` by one level for a term that goes into a new query.
fn raise(query: &mut Query) {
    query.each_expr_mut(0, &mut |expr, depth| expr.raise_from(1, depth + 1));
}

/// `rewriteSearchAndCycle`: the query of `WITH` that gives the columns of the clauses. The left term becomes a query that reads the old term and adds the first values of the columns. The right term gives the columns of the reference to the query too, and a query that reads it adds the next values. With `CYCLE`, the right term does not read a row with the mark of a cycle.
pub(crate) fn rewrite(
    name: &str,
    columns: &[Column],
    work: Option<usize>,
    search: Option<&SearchClause>,
    cycle: Option<&CycleClause>,
    query: &Query,
) -> Result<Query> {
    let mut new = query.clone();
    let Some(op) = new.set_op.as_mut() else {
        return Err(Error::internal("a recursive query with no set operation"));
    };
    let (SetTree::Leaf(left), SetTree::Leaf(right)) = (&op.left, &op.right) else {
        return Err(Error::internal("a term of SEARCH or CYCLE that is not a SELECT"));
    };
    let (left, right) = (*left, *right);
    let width = columns.len();
    let extras = extra_columns(search, cycle);
    let mut all = columns.to_vec();
    all.extend(extras.iter().cloned());
    let plain: Vec<Target> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| target(&c.name, var(0, i, c.ty, c.typmod)))
        .collect();

    // The left term: `SELECT cols, extras FROM (q1) "*TLOCRN*"(cols)`.
    let Some(q1) = new.relations[left].subquery.as_deref() else {
        return Err(Error::internal("a SELECT of a set operation with no query"));
    };
    let mut q1 = q1.clone();
    raise(&mut q1);
    let mut targets = plain.clone();
    if let Some(s) = search {
        let args = row_of(&s.columns, columns);
        let expr = if s.breadth {
            let zero = Expr::constant(Value::Int8(0), oid::INT8, -1, None);
            row(std::iter::once(zero).chain(args).collect())
        } else {
            array(args)
        };
        targets.push(target(&s.seq, expr));
    }
    if let Some(c) = cycle {
        targets.push(target(&c.mark, c.default.clone()));
        targets.push(target(&c.path, array(row_of(&c.columns, columns))));
    }
    let newq1 = wrap("*TLOCRN*", columns.to_vec(), q1, targets);

    // The right term: `SELECT cols, next extras FROM (q2 with the extras of the reference) "*TROCRN*"(cols, extras) WHERE mark <> value`.
    let Some(q2) = new.relations[right].subquery.as_deref() else {
        return Err(Error::internal("a SELECT of a set operation with no query"));
    };
    let mut q2 = q2.clone();
    let reference = q2.relations.iter().position(|r| {
        r.cte.as_ref().is_some_and(|c| c.name == name && c.work.is_some() && c.work == work)
    });
    let Some(reference) = reference else {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!(
                "with a SEARCH or CYCLE clause, the recursive reference to WITH query \"{name}\" must be at the top level of its right-hand SELECT"
            ),
        ));
    };
    if q2.targets.iter().any(|t| t.junk) {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "with a SEARCH or CYCLE clause, a right-hand SELECT with a junk column is not supported yet",
        ));
    }
    raise(&mut q2);
    for (k, extra) in extras.iter().enumerate() {
        q2.targets.push(target(&extra.name, var(reference, width + k, extra.ty, extra.typmod)));
    }
    let mut targets = plain;
    let mut next = width;
    if let Some(s) = search {
        let args = row_of(&s.columns, columns);
        let expr = if s.breadth {
            let seq = var(0, next, oid::RECORD, -1);
            let depth = Expr::new(ExprKind::FieldSelect(Box::new(seq), 0), oid::INT8);
            let depth = call(INT8INC, vec![depth], oid::INT8);
            row(std::iter::once(depth).chain(args).collect())
        } else {
            path_cat(args, next)
        };
        targets.push(target(&s.seq, expr));
        next += 1;
    }
    let mut filter = None;
    if let Some(c) = cycle {
        let (mark, path) = (next, next + 1);
        let eq = builtin::operator_by_oid(RECORD_EQ_OP).map_or(0, |o| o.code);
        let path_var = var(0, path, oid::RECORD_ARRAY, -1);
        let args = vec![row(row_of(&c.columns, columns)), path_var];
        let found = Expr::new(ExprKind::ScalarArrayOp { func: eq, any: true, args }, oid::BOOL);
        let case = Case {
            arg: None,
            whens: vec![(found, c.value.clone())],
            default: Box::new(c.default.clone()),
        };
        let case = Expr { typmod: c.typmod, ..Expr::new(ExprKind::Case(case), c.ty) };
        targets.push(target(&c.mark, case));
        targets.push(target(&c.path, path_cat(row_of(&c.columns, columns), path)));
        let (ne, func) = c.ne;
        let args = vec![var(0, mark, c.ty, c.typmod), c.value.clone()];
        let ne =
            Func { oid: func, args, form: FuncForm::Operator(ne), variadic: false, retset: false };
        filter = Some(Expr::new(ExprKind::Func(ne), oid::BOOL));
    }
    let mut newq2 = wrap("*TROCRN*", all, q2, targets);
    newq2.filter = filter;

    // The set operation and the query of `WITH` give the extras too.
    for (k, extra) in extras.iter().enumerate() {
        op.columns.push((extra.ty, extra.typmod));
        if !op.all {
            let mut group = set_group(width + k, extra.ty, None)?;
            if matches!(extra.ty, oid::RECORD | oid::RECORD_ARRAY) {
                group.hashable = true;
            }
            op.groups.push(group);
        }
    }
    for (index, sub) in [(left, newq1), (right, newq2)] {
        let relation = &mut new.relations[index];
        relation.subquery = Some(Box::new(sub));
        relation.columns.extend(extras.iter().cloned());
    }
    for (k, extra) in extras.iter().enumerate() {
        new.targets.push(target(&extra.name, var(left, width + k, extra.ty, extra.typmod)));
    }
    Ok(new)
}

impl Analyzer<'_> {
    /// The part of `analyzeCTE` before the analysis of the query: the type of the mark from its two values, and the `<>` operator of the type.
    pub(crate) fn cycle_clause(&mut self, raw: &CTECycleClause) -> Result<CycleClause> {
        let value = self.transform(raw.cycle_mark_value.as_ref())?;
        let default = self.transform(raw.cycle_mark_default.as_ref())?;
        let ty = self.common_type(&[value.clone(), default.clone()], "CYCLE")?;
        let value = self.coerce_to_common(value, ty, "CYCLE/SET/TO")?;
        let default = self.coerce_to_common(default, ty, "CYCLE/SET/DEFAULT")?;
        let typmod = if value.typmod == default.typmod { value.typmod } else { -1 };
        let eq = typcache::operators(ty).eq;
        if eq == 0 {
            return Err(Error::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("could not identify an equality operator for type {}", types::name(ty)),
            ));
        }
        let ne = builtin::operator_by_oid(eq).map_or(0, |o| o.negator);
        let Some(func) = builtin::operator_by_oid(ne).map(|o| o.code).filter(|&f| f != 0) else {
            return Err(Error::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("could not identify an inequality operator for type {}", types::name(ty)),
            ));
        };
        Ok(CycleClause {
            columns: strings(&raw.cycle_col_list),
            mark: raw.cycle_mark_column.as_deref().unwrap_or_default().to_string(),
            value,
            default,
            ty,
            typmod,
            path: raw.cycle_path_column.as_deref().unwrap_or_default().to_string(),
            ne: (ne, func),
            location: place(raw.location),
        })
    }
}
