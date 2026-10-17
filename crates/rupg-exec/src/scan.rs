//! The scan of the relations and the joins of `FROM`. A table of the catalog gives its static rows, then the rows of the user objects of the session. A relation of the user gives the rows of [`user_rows`]. The rows of a subquery or a function in `FROM` come from the executor, which adds them to [`Tables`]. A subquery or a function that reads the relations before it, as `LATERAL` lets it, runs in the scan for each row of those relations.
//!
//! A tuple of the join is the row number of each relation of the query, or [`NONE`] for a relation that a row of an outer join does not have. The rows of each relation are read once, with only the columns that the query uses.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use rupg_analyze::{
    Expr, ExprKind, FromFunction, FromItem, JoinKind, Query, TABLE_OID_ATTNUM, Var,
};
use rupg_common::{Error, Result};
use rupg_func::Session;
use rupg_pgcatalog::{Batch, Values};
use rupg_types::{Array, Record, Value, oid};

/// The attribute number of the system column `xmin`.
const XMIN_ATTNUM: i16 = -2;

/// `FrozenTransactionId`: the transaction ID of a row that each snapshot sees.
const FROZEN_TRANSACTION_ID: u32 = 2;

/// The OID of the catalog `pg_authid`.
const PG_AUTHID: u32 = 1260;

/// The index of the column `rolpassword` of `pg_authid`. The static rows have no password, and the session gives the stored secret of each role.
const ROLPASSWORD: usize = 10;

/// The row number of a relation that is null in a tuple.
pub(crate) const NONE: usize = usize::MAX;

/// The functions of the `=` operators that a hash join can use, because two values are equal only when they have the same form: `booleq`, `chareq`, `nameeq`, `int2eq`, `int4eq`, `texteq`, `oideq` and `int8eq`.
pub(crate) const HASH_EQUALS: [u32; 8] = [60, 61, 62, 63, 65, 67, 184, 467];

/// The rows of the relations of a query.
#[derive(Clone, Default)]
pub(crate) struct Tables {
    /// The rows of each relation, with a null for each column that the query does not use.
    pub(crate) rows: Vec<Rc<Vec<Vec<Value>>>>,
    /// The OID of each relation, for `tableoid`.
    pub(crate) oids: Vec<u32>,
    /// The types of the columns of each relation, for the value of a whole-row reference.
    types: Vec<Vec<u32>>,
    /// The names of the columns of each relation, for the value of a whole-row reference.
    names: Vec<Option<Arc<[String]>>>,
    /// For each relation, the other relations of the query that it reads, as [`lateral`] finds them.
    pub(crate) needs: Vec<Vec<usize>>,
    /// The rows of each relation that reads other relations. The scan adds the rows of each run.
    grown: Vec<RefCell<Vec<Vec<Value>>>>,
    /// The index of a column, by the relation and the attribute number, that the scan makes for the first run of a correlated subquery and uses again in the next runs.
    indexes: RefCell<Indexes>,
}

/// The row numbers of a relation for each value of a column.
type Index = BTreeMap<Key, Vec<usize>>;

/// The indexes of the columns, by the relation and the attribute number.
type Indexes = Vec<((usize, i16), Rc<Index>)>;

/// For each relation of the query, the other relations of the query that its subquery or its function reads, as `LATERAL` lets it.
pub(crate) fn lateral(query: &Query) -> Vec<Vec<usize>> {
    let refs = |sub: &Query| {
        let mut out = Vec::new();
        for (expr, depth) in sub.all_exprs(0) {
            expr.find(depth, &mut |e, depth| {
                if let ExprKind::Var(var) = e.kind
                    && var.levels_up == depth + 1
                    && !out.contains(&var.relation)
                {
                    out.push(var.relation);
                }
                None::<()>
            });
        }
        out
    };
    let calls = |function: &FromFunction| {
        let mut out = Vec::new();
        for (call, _) in &function.calls {
            each_var(call, &mut |var| {
                if !out.contains(&var.relation) {
                    out.push(var.relation);
                }
            });
        }
        out
    };
    let values = |rows: &Vec<Vec<Expr>>| {
        let mut out = Vec::new();
        for expr in rows.iter().flatten() {
            each_var(expr, &mut |var| {
                if !out.contains(&var.relation) {
                    out.push(var.relation);
                }
            });
        }
        out
    };
    query
        .relations
        .iter()
        .map(|r| match (&r.subquery, &r.function, &r.values) {
            (Some(sub), _, _) => refs(sub),
            (None, Some(function), _) => calls(function),
            (None, None, Some(rows)) => values(rows),
            (None, None, None) => Vec::new(),
        })
        .collect()
}

impl Tables {
    /// The value of a column in a tuple.
    pub(crate) fn var(&self, tuple: &[usize], var: Var) -> Value {
        let row = tuple.get(var.relation).copied().unwrap_or(NONE);
        if row == NONE {
            return Value::Null;
        }
        if var.attnum == TABLE_OID_ATTNUM {
            return Value::Oid(self.oids[var.relation]);
        }
        // rupg has no row versions yet, so each row shows `xmin`, `cmin`, `xmax` and `cmax` of a frozen row that no transaction deleted.
        if var.attnum == XMIN_ATTNUM {
            return Value::Oid(FROZEN_TRANSACTION_ID);
        }
        if var.attnum < 0 {
            return Value::Oid(0);
        }
        if var.attnum == 0 {
            let values = if self.needs[var.relation].is_empty() {
                self.rows[var.relation][row].clone()
            } else {
                self.grown[var.relation].borrow()[row].clone()
            };
            let types = self.types[var.relation].clone();
            let names = self.names[var.relation].clone();
            return Value::Record(Box::new(Record { types, values, names }));
        }
        let column = usize::try_from(var.attnum - 1).unwrap_or(usize::MAX);
        if self.needs[var.relation].is_empty() {
            return self.rows[var.relation][row].get(column).cloned().unwrap_or(Value::Null);
        }
        let grown = self.grown[var.relation].borrow();
        grown[row].get(column).cloned().unwrap_or(Value::Null)
    }

    /// The tuples of a relation that reads other relations, for one row of those relations: the rows of a new run, which the relation keeps.
    fn run(
        &self,
        index: usize,
        context: &[usize],
        width: usize,
        source: &mut Source<'_>,
    ) -> Result<Vec<Vec<usize>>> {
        let rows = source(index, context)?;
        let mut grown = self.grown[index].borrow_mut();
        let start = grown.len();
        grown.extend(rows);
        Ok((start..grown.len())
            .map(|row| {
                let mut tuple = vec![NONE; width];
                tuple[index] = row;
                tuple
            })
            .collect())
    }

    /// The index of a column of a relation that reads no other relation. The first call makes it and the next calls use it again.
    fn index(&self, var: Var) -> Rc<Index> {
        let column = (var.relation, var.attnum);
        if let Some((_, index)) = self.indexes.borrow().iter().find(|(c, _)| *c == column) {
            return Rc::clone(index);
        }
        let mut index = Index::new();
        let mut tuple = vec![NONE; self.rows.len()];
        for row in 0..self.rows[var.relation].len() {
            tuple[var.relation] = row;
            if let Some(k) = key(self.var(&tuple, var)) {
                index.entry(k).or_default().push(row);
            }
        }
        let index = Rc::new(index);
        self.indexes.borrow_mut().push((column, Rc::clone(&index)));
        index
    }

    /// True when a relation of the part reads a relation outside the part.
    fn depends(&self, relations: &[usize]) -> bool {
        relations.iter().any(|r| self.needs[*r].iter().any(|n| !relations.contains(n)))
    }
}

/// Calls `f` for each column of the query that an expression reads, also in a subquery. A column of an outer query does not count.
fn each_var(expr: &Expr, f: &mut impl FnMut(Var)) {
    expr.find(0, &mut |e, depth| {
        if let ExprKind::Var(var) = e.kind
            && var.levels_up == depth
        {
            f(var);
        }
        None::<()>
    });
}

/// The relations that an expression reads.
fn relations_of(expr: &Expr) -> Vec<usize> {
    let mut relations = Vec::new();
    each_var(expr, &mut |var| {
        if !relations.contains(&var.relation) {
            relations.push(var.relation);
        }
    });
    relations
}

/// The relations of a part of `FROM`.
fn relations_in(item: &FromItem, out: &mut Vec<usize>) {
    match item {
        FromItem::Relation(index) => out.push(*index),
        FromItem::Join(join) => {
            relations_in(&join.left, out);
            relations_in(&join.right, out);
        }
    }
}

/// Reads the rows of the relations of a query.
///
/// # Errors
///
/// The error of the input function of a column that the catalog keeps as text.
pub(crate) fn read(query: &Query, session: &dyn Session) -> Result<Tables> {
    let mut used: Vec<Vec<bool>> =
        query.relations.iter().map(|r| vec![false; r.columns.len()]).collect();
    // The expressions of the subqueries in `FROM` count too, because a `LATERAL` subquery can read the columns of the query.
    for (expr, depth) in query.all_exprs(0) {
        expr.find(depth, &mut |e, depth| {
            if let ExprKind::Var(var) = e.kind
                && var.levels_up == depth
            {
                // A whole-row reference reads all the columns.
                match usize::try_from(var.attnum - 1) {
                    Ok(i) => {
                        if let Some(slot) = used[var.relation].get_mut(i) {
                            *slot = true;
                        }
                    }
                    Err(_) if var.attnum == 0 => used[var.relation].fill(true),
                    Err(_) => {}
                }
            }
            None::<()>
        });
    }
    let mut rows = Vec::with_capacity(query.relations.len());
    for (relation, used) in query.relations.iter().zip(&used) {
        let work = relation.cte.as_ref().is_some_and(|c| c.work.is_some());
        if relation.subquery.is_some()
            || relation.function.is_some()
            || relation.values.is_some()
            || work
        {
            rows.push(Rc::default());
            continue;
        }
        let Some(catalog) = rupg_pgcatalog::catalog_by_oid(relation.oid) else {
            rows.push(Rc::new(user_rows(relation.oid, session)?));
            continue;
        };
        let mut table = Vec::with_capacity(catalog.len);
        for row in 0..catalog.len {
            let mut values = Vec::with_capacity(used.len());
            for (i, column) in relation.columns.iter().enumerate() {
                values.push(if used[i] {
                    value(&catalog.rows[i], row, column.ty, session)?
                } else {
                    Value::Null
                });
            }
            table.push(values);
        }
        if relation.oid == PG_AUTHID && used.get(ROLPASSWORD).copied().unwrap_or(false) {
            for (row, values) in table.iter_mut().enumerate() {
                if let Values::Oid(roles) = catalog.rows[0].values
                    && let Some(password) = session.role_password(roles[row])
                {
                    values[ROLPASSWORD] = Value::text(password);
                }
            }
        }
        if let Some(user) = session.catalog() {
            table.extend(crate::user::rows(catalog, user, session)?);
        }
        rows.push(Rc::new(table));
    }
    Ok(Tables {
        rows,
        oids: query.relations.iter().map(|r| r.oid).collect(),
        types: query.relations.iter().map(|r| r.columns.iter().map(|c| c.ty).collect()).collect(),
        names: query.relations.iter().map(row_names).collect(),
        needs: lateral(query),
        grown: query.relations.iter().map(|_| RefCell::default()).collect(),
        indexes: RefCell::default(),
    })
}

/// The names of the fields of a whole row of a relation. A subquery, a function or a `VALUES` list takes the names of the alias, then the names of its columns. A table takes the names of the columns of its row type, as the alias does not change the row type.
fn row_names(relation: &rupg_analyze::Relation) -> Option<Arc<[String]>> {
    let alias = if relation.oid == 0 { relation.alias.as_deref().unwrap_or_default() } else { &[] };
    let names = relation.columns.iter().enumerate();
    Some(names.map(|(i, c)| alias.get(i).unwrap_or(&c.name).clone()).collect())
}

/// The rows of a relation of the user. A session cannot add rows yet, so a table has no rows. A sequence has its one row, the row of its file. The read of a table counts as a sequential scan, because rupg has no index scans yet.
fn user_rows(oid: u32, session: &dyn Session) -> Result<Vec<Vec<Value>>> {
    let relation = session
        .catalog()
        .and_then(|c| c.relation(oid))
        .ok_or_else(|| Error::internal(format!("no relation has the OID {oid}")))?;
    if relation.kind == rupg_catalog::RelKind::Table {
        session.count_scan(oid);
        // A row from before an `ALTER TABLE ... ADD COLUMN` has no value for the new columns, which are null.
        let width = relation.columns.len();
        let mut rows = session.table_rows(oid)?;
        for row in &mut rows {
            row.resize(width, Value::Null);
        }
        return Ok(rows);
    }
    Ok(match &relation.sequence {
        Some(sequence) => {
            let row = session.sequence_row(sequence.file).unwrap_or(sequence.data);
            vec![vec![
                Value::Int8(row.last_value),
                Value::Int8(row.log_cnt),
                Value::Bool(row.is_called),
            ]]
        }
        None => Vec::new(),
    })
}

/// The value of a row of a column of the catalog. The elements of an array that the catalog keeps as text, such as `conkey`, get the element type of the column.
pub(crate) fn value(batch: &Batch, row: usize, ty: u32, session: &dyn Session) -> Result<Value> {
    if batch.is_null(row) {
        return Ok(Value::Null);
    }
    Ok(match batch.values {
        Values::Null => Value::Null,
        Values::Bool(v) => Value::Bool(v[row]),
        Values::Char(v) => Value::Char(v[row]),
        Values::Int2(v) => Value::Int2(v[row]),
        Values::Int4(v) => Value::Int4(v[row]),
        Values::Float4(v) => Value::Float4(v[row]),
        Values::Oid(v) => Value::Oid(v[row]),
        Values::Text(v) => match ty {
            oid::NAME | oid::TEXT | oid::PG_NODE_TREE | oid::ANYARRAY => Value::text(v[row]),
            oid::ACLITEM_ARRAY => {
                let array = rupg_types::array_in(v[row], b',', true, |s| Ok(Value::text(s)))
                    .map_err(|e| Error::internal(e.message))?;
                Value::Array(Box::new(array))
            }
            _ => rupg_func::input(ty, v[row], -1, session)?,
        },
        Values::Int2Vector(v) => vector(v[row].iter().map(|&n| Value::Int2(n)), true),
        Values::OidVector(v) => vector(v[row].iter().map(|&n| Value::Oid(n)), true),
        Values::OidArray(v) => vector(v[row].iter().map(|&n| Value::Oid(n)), false),
        Values::CharArray(v) => vector(v[row].iter().map(|&n| Value::Char(n)), false),
        Values::TextArray(v) => {
            let element = rupg_pgcatalog::builtin::type_by_oid(ty).map_or(0, |row| row.elem);
            let values: Result<Vec<Value>> = v[row]
                .iter()
                .map(|&s| match element {
                    0 | oid::TEXT | oid::NAME | oid::ACLITEM => Ok(Value::text(s)),
                    _ => rupg_func::input(element, s, -1, session),
                })
                .collect();
            vector(values?.into_iter(), false)
        }
    })
}

/// An array of values: an `int2vector` or an `oidvector` from 0 when `from_zero` is true, or else an array from 1.
fn vector(values: impl Iterator<Item = Value>, from_zero: bool) -> Value {
    let values: Vec<Option<Value>> = values.map(Some).collect();
    let array = if from_zero { Array::vector(values) } else { Array::one(values) };
    Value::Array(Box::new(array))
}

/// The key of a value for a hash join, or `None` for a null, which equals no value.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Key {
    Int(i64),
    Text(String),
}

pub(crate) fn key(value: Value) -> Option<Key> {
    Some(match value {
        Value::Bool(v) => Key::Int(i64::from(v)),
        Value::Char(v) => Key::Int(i64::from(v)),
        Value::Int2(v) => Key::Int(i64::from(v)),
        Value::Int4(v) => Key::Int(i64::from(v)),
        Value::Int8(v) => Key::Int(v),
        Value::Oid(v) => Key::Int(i64::from(v)),
        Value::Text(v) => Key::Text(v),
        _ => return None,
    })
}

/// The column of an expression that is a column, or a column with another type of the same form.
fn plain_var(expr: &Expr) -> Option<Var> {
    match &expr.kind {
        ExprKind::Var(var) if var.levels_up == 0 => Some(*var),
        ExprKind::Relabel(arg, _) => plain_var(arg),
        _ => None,
    }
}

/// True when an expression is a column of an outer query, or such a column with another type of the same form.
fn outer_var(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Var(var) => var.levels_up > 0,
        ExprKind::Relabel(arg, _) => outer_var(arg),
        _ => false,
    }
}

/// The parts of a condition that `AND` joins. They are references and not copies, because the cache of the rows of a subquery keeps them by the address of the subquery: a copy that the scan drops can free an address that another subquery then gets.
fn conjuncts<'q>(expr: &'q Expr, out: &mut Vec<&'q Expr>) {
    match &expr.kind {
        ExprKind::Bool(rupg_analyze::BoolOp::And, args) => {
            for arg in args {
                conjuncts(arg, out);
            }
        }
        _ => out.push(expr),
    }
}

/// A join: the tuples of both sides, the condition, the kind and the relations of each side.
struct JoinInput<'a> {
    left: Vec<Vec<usize>>,
    right: Vec<Vec<usize>>,
    on: &'a [&'a Expr],
    kind: JoinKind,
    left_relations: &'a [usize],
    right_relations: &'a [usize],
}

/// The value of an expression for a tuple. A condition passes when it gives true.
type Test<'a> = dyn FnMut(&Expr, &[usize]) -> Result<Value> + 'a;

/// The run of a relation that reads other relations: the rows of the relation for a tuple of the relations that it reads.
pub(crate) type Source<'a> = dyn FnMut(usize, &[usize]) -> Result<Vec<Vec<Value>>> + 'a;

/// The tuple with the rows of `inner`, and the rows of `outer` for the relations that `inner` does not have.
fn merged(outer: &[usize], inner: &[usize]) -> Vec<usize> {
    outer.iter().zip(inner).map(|(o, i)| if *i == NONE { *o } else { *i }).collect()
}

/// The scan of the parts of `FROM`.
struct Scan<'a, 'b> {
    tables: &'a Tables,
    width: usize,
    test: &'a mut Test<'b>,
    source: &'a mut Source<'b>,
}

/// True when the relations of the expression are all in the list.
fn covered(expr: &Expr, relations: &[usize]) -> bool {
    relations_of(expr).iter().all(|r| relations.contains(r))
}

/// The tuples that pass every condition.
fn keep(tuples: Vec<Vec<usize>>, conds: &[&Expr], test: &mut Test<'_>) -> Result<Vec<Vec<usize>>> {
    if conds.is_empty() {
        return Ok(tuples);
    }
    let mut kept = Vec::with_capacity(tuples.len());
    'tuple: for tuple in tuples {
        for expr in conds {
            if test(expr, &tuple)? != Value::Bool(true) {
                continue 'tuple;
            }
        }
        kept.push(tuple);
    }
    Ok(kept)
}

/// The tuples of the query before the target list: the parts of `FROM` joined with a cross join, and the `WHERE` condition. Each part of `WHERE` that `AND` joins goes down to the smallest part of `FROM` that has its relations, where the query can test it on fewer rows. `test` gives true when a condition is true for a tuple.
///
/// # Errors
///
/// The error of a condition.
pub(crate) fn tuples(
    query: &Query,
    tables: &Tables,
    test: &mut Test<'_>,
    source: &mut Source<'_>,
) -> Result<Vec<Vec<usize>>> {
    let width = query.relations.len();
    let mut scan = Scan { tables, width, test, source };
    let mut filter = Vec::new();
    if let Some(expr) = &query.filter {
        conjuncts(expr, &mut filter);
    }
    let items: Vec<(&FromItem, Vec<usize>)> = query
        .from
        .iter()
        .map(|item| {
            let mut relations = Vec::new();
            relations_in(item, &mut relations);
            (item, relations)
        })
        .collect();
    // A condition with no relation stays at the top, so that it runs once for each tuple as in a query with no FROM.
    let mut own: Vec<Vec<&Expr>> = vec![Vec::new(); items.len()];
    let mut rest = Vec::new();
    for expr in filter {
        let relations = relations_of(expr);
        match items.iter().position(|(_, r)| !relations.is_empty() && covered(expr, r)) {
            Some(i) => own[i].push(expr),
            None => rest.push(expr),
        }
    }
    let mut acc: Vec<Vec<usize>> = vec![vec![NONE; width]];
    let mut acc_relations: Vec<usize> = Vec::new();
    for ((item, relations), conds) in items.iter().zip(own) {
        let mut all = acc_relations.clone();
        all.extend(relations);
        let mut on = Vec::new();
        rest.retain(|expr| {
            let relations = relations_of(expr);
            if !relations.is_empty() && covered(expr, &all) {
                on.push(*expr);
                false
            } else {
                true
            }
        });
        let input = JoinInput {
            left: acc,
            right: Vec::new(),
            on: &on,
            kind: JoinKind::Inner,
            left_relations: &acc_relations,
            right_relations: relations,
        };
        acc = scan.join_item(input, item, conds, &vec![NONE; width])?;
        acc_relations = all;
    }
    keep(acc, &rest, scan.test)
}

impl Scan<'_, '_> {
    /// The join of the tuples `input.left` with the tuples of the part `item`, which pass the conditions `conds`. When the part reads relations of the left side, as `LATERAL` lets it, the part runs again for each tuple of the left side, as a nested loop with parameters does. `context` has the rows of the relations outside the join.
    fn join_item<'q>(
        &mut self,
        mut input: JoinInput<'_>,
        item: &'q FromItem,
        conds: Vec<&'q Expr>,
        context: &[usize],
    ) -> Result<Vec<Vec<usize>>> {
        if !self.tables.depends(input.right_relations) {
            input.right = self.item_tuples(item, conds, context)?;
            return join(input, self.tables, self.test);
        }
        let left = std::mem::take(&mut input.left);
        let mut out = Vec::new();
        for l in left {
            let context = merged(context, &l);
            let right = self.item_tuples(item, conds.clone(), &context)?;
            let input = JoinInput { left: vec![l], right, ..input };
            out.extend(join(input, self.tables, self.test)?);
        }
        Ok(out)
    }

    /// The rows of a relation that can pass the conditions `conds`, through an index of a column, when a condition is an `=` of the column and a column of an outer query, as in a correlated subquery. The subquery runs for each row of the outer query, and the index makes each run read only the rows with the value of that row, as an index scan with a parameter does. `None` when no condition is such an `=`.
    fn lookup(&mut self, relation: usize, conds: &[&Expr]) -> Result<Option<Vec<usize>>> {
        let found = conds.iter().find_map(|expr| {
            let ExprKind::Func(f) = &expr.kind else { return None };
            if !HASH_EQUALS.contains(&f.oid) || f.args.len() != 2 {
                return None;
            }
            match (plain_var(&f.args[0]), plain_var(&f.args[1])) {
                (Some(var), None) if var.relation == relation && outer_var(&f.args[1]) => {
                    Some((var, &f.args[1]))
                }
                (None, Some(var)) if var.relation == relation && outer_var(&f.args[0]) => {
                    Some((var, &f.args[0]))
                }
                _ => None,
            }
        });
        let Some((var, outer)) = found else { return Ok(None) };
        // A null equals no value.
        let Some(k) = key((self.test)(outer, &vec![NONE; self.width])?) else {
            return Ok(Some(Vec::new()));
        };
        Ok(Some(self.tables.index(var).get(&k).cloned().unwrap_or_default()))
    }

    /// The tuples of a part of `FROM` that pass the conditions `conds`, which read only the relations of the part. `context` has the rows of the relations outside the part.
    fn item_tuples<'q>(
        &mut self,
        item: &'q FromItem,
        conds: Vec<&'q Expr>,
        context: &[usize],
    ) -> Result<Vec<Vec<usize>>> {
        let width = self.width;
        let j = match item {
            FromItem::Relation(index) if !self.tables.needs[*index].is_empty() => {
                let tuples = self.tables.run(*index, context, width, self.source)?;
                return keep(tuples, &conds, self.test);
            }
            FromItem::Relation(index) => {
                let rows = match self.lookup(*index, &conds)? {
                    Some(rows) => rows,
                    None => (0..self.tables.rows[*index].len()).collect(),
                };
                let tuples = rows
                    .into_iter()
                    .map(|row| {
                        let mut tuple = vec![NONE; width];
                        tuple[*index] = row;
                        tuple
                    })
                    .collect();
                return keep(tuples, &conds, self.test);
            }
            FromItem::Join(j) => j,
        };
        let (mut left_relations, mut right_relations) = (Vec::new(), Vec::new());
        relations_in(&j.left, &mut left_relations);
        relations_in(&j.right, &mut right_relations);
        let mut on = Vec::new();
        if let Some(expr) = &j.on {
            conjuncts(expr, &mut on);
        }
        // A condition on one side goes down to that side when the join keeps every row of that side. The other conditions of an inner join join the condition of the join, and those of an outer join test its result.
        let (mut to_left, mut to_right, mut after) = (Vec::new(), Vec::new(), Vec::new());
        let push_left = matches!(j.kind, JoinKind::Inner | JoinKind::Left);
        let push_right = matches!(j.kind, JoinKind::Inner | JoinKind::Right);
        for expr in conds {
            let relations = relations_of(expr);
            let nonempty = !relations.is_empty();
            if push_left && nonempty && covered(expr, &left_relations) {
                to_left.push(expr);
            } else if push_right && nonempty && covered(expr, &right_relations) {
                to_right.push(expr);
            } else if j.kind == JoinKind::Inner {
                on.push(expr);
            } else {
                after.push(expr);
            }
        }
        // A condition of ON that reads one side only goes down to that side for an inner join, and to the side that can be null for an outer join.
        if j.kind != JoinKind::Full {
            let mut kept = Vec::with_capacity(on.len());
            for expr in on {
                let relations = relations_of(expr);
                let nonempty = !relations.is_empty();
                if nonempty && j.kind != JoinKind::Left && covered(expr, &left_relations) {
                    to_left.push(expr);
                } else if nonempty && j.kind != JoinKind::Right && covered(expr, &right_relations) {
                    to_right.push(expr);
                } else {
                    kept.push(expr);
                }
            }
            on = kept;
        }
        let left = self.item_tuples(&j.left, to_left, context)?;
        let input = JoinInput {
            left,
            right: Vec::new(),
            on: &on,
            kind: j.kind,
            left_relations: &left_relations,
            right_relations: &right_relations,
        };
        let tuples = self.join_item(input, &j.right, to_right, context)?;
        keep(tuples, &after, self.test)
    }
}

/// One side of an `=` that a hash join tests: a column, or an expression that reads only the relations of that side.
enum Side<'q> {
    Var(Var),
    Expr(&'q Expr),
}

/// A join of two lists of tuples: a join through an index of the right side when the condition has an `=` that a hash can test, with a column or an expression of each side, or else a nested loop. The rows of an outer join that match no row get nulls for the other side.
fn join<'a>(input: JoinInput<'a>, tables: &Tables, test: &mut Test<'_>) -> Result<Vec<Vec<usize>>> {
    let JoinInput { left, right, on, kind, left_relations, right_relations } = input;
    let merge = |l: &[usize], r: &[usize]| -> Vec<usize> {
        let mut tuple = l.to_vec();
        for &rel in right_relations {
            tuple[rel] = r[rel];
        }
        tuple
    };
    // The first `=` with a column of each side, else the first `=` with an expression of each side. A column needs no evaluation.
    let side = |e: &'a Expr, relations: &[usize]| -> Option<Side<'a>> {
        match plain_var(e) {
            Some(var) => relations.contains(&var.relation).then_some(Side::Var(var)),
            None => {
                let used = relations_of(e);
                (!used.is_empty() && used.iter().all(|r| relations.contains(r)))
                    .then_some(Side::Expr(e))
            }
        }
    };
    let pairs: Vec<(Side<'a>, Side<'a>)> = on
        .iter()
        .filter_map(|expr| {
            let ExprKind::Func(f) = &expr.kind else { return None };
            if !HASH_EQUALS.contains(&f.oid) || f.args.len() != 2 {
                return None;
            }
            let (a, b) = (&f.args[0], &f.args[1]);
            match (side(a, left_relations), side(b, right_relations)) {
                (Some(l), Some(r)) => Some((l, r)),
                _ => Some((side(b, left_relations)?, side(a, right_relations)?)),
            }
        })
        .collect();
    let hash = pairs
        .iter()
        .find(|(l, r)| matches!((l, r), (Side::Var(_), Side::Var(_))))
        .or_else(|| pairs.first());
    let mut value = |tuple: &[usize], side: &Side<'_>| -> Result<Value> {
        match side {
            Side::Var(var) => Ok(tables.var(tuple, *var)),
            Side::Expr(e) => test(e, tuple),
        }
    };
    let mut right_matched = vec![false; right.len()];
    let mut out = Vec::new();
    let mut index: BTreeMap<Key, Vec<usize>> = BTreeMap::new();
    let mut left_keys = Vec::new();
    if let Some((l, r)) = hash
        && !left.is_empty()
    {
        for (i, tuple) in right.iter().enumerate() {
            if let Some(k) = key(value(tuple, r)?) {
                index.entry(k).or_default().push(i);
            }
        }
        for tuple in &left {
            left_keys.push(key(value(tuple, l)?));
        }
    }
    let all: Vec<usize> = (0..right.len()).collect();
    for (n, l) in left.iter().enumerate() {
        let candidates: &[usize] = match hash {
            Some(_) => match &left_keys[n] {
                Some(k) => index.get(k).map_or(&[], Vec::as_slice),
                None => &[],
            },
            None => &all,
        };
        let mut matched = false;
        for &i in candidates {
            let tuple = merge(l, &right[i]);
            let mut pass = true;
            for expr in on {
                if test(expr, &tuple)? != Value::Bool(true) {
                    pass = false;
                    break;
                }
            }
            if pass {
                matched = true;
                right_matched[i] = true;
                out.push(tuple);
            }
        }
        if !matched && matches!(kind, JoinKind::Left | JoinKind::Full) {
            out.push(l.clone());
        }
    }
    if matches!(kind, JoinKind::Right | JoinKind::Full) {
        for (i, r) in right.iter().enumerate() {
            if !right_matched[i] {
                let mut tuple = vec![NONE; r.len()];
                for &rel in right_relations {
                    tuple[rel] = r[rel];
                }
                out.push(tuple);
            }
        }
    }
    Ok(out)
}
