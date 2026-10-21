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
use rupg_common::{Error, Result, SqlState};
use rupg_func::Session;
use rupg_pgcatalog::{Batch, Values};
use rupg_types::{Array, Record, Value, oid};

/// The attribute number of the system column `xmin`.
const XMIN_ATTNUM: i16 = -2;

/// `SelfItemPointerAttributeNumber`: the attribute number of `ctid`.
const CTID_ATTNUM: i16 = -1;

/// `FrozenTransactionId`: the transaction ID of a row that each snapshot sees.
const FROZEN_TRANSACTION_ID: u32 = 2;

/// The OID of the catalog `pg_authid`.
const PG_AUTHID: u32 = 1260;

/// The index of the column `rolpassword` of `pg_authid`. The static rows have no password, and the session gives the stored secret of each role.
const ROLPASSWORD: usize = 10;

/// The row number of a relation that is null in a tuple.
pub(crate) const NONE: usize = usize::MAX;

/// The most bytes that the tuples of one join can use. The executor keeps all the tuples of a join in memory, so a join with more tuples gives an error and does not stop the server. The tests use a smaller limit.
const JOIN_BYTES: usize = if cfg!(test) { 1 << 22 } else { 1 << 30 };

/// The most tuples of `width` relations that one join can make.
fn join_limit(width: usize) -> usize {
    JOIN_BYTES / (size_of::<Vec<usize>>() + width * size_of::<usize>())
}

/// The error of a join with more tuples than `join_limit` lets it make.
fn too_many(limit: usize) -> Error {
    Error::new(SqlState::OUT_OF_MEMORY, "out of memory").with_detail(format!(
        "The join makes more than {limit} rows, and rupg keeps all the rows of a join in memory."
    ))
}

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
    /// One row of the relation `oid` with the column types `types`, for the expressions of [`crate::evaluate_row`].
    pub(crate) fn one_row(oid: u32, types: Vec<u32>, row: Vec<Value>) -> Tables {
        Tables {
            rows: vec![Rc::new(vec![row])],
            oids: vec![oid],
            types: vec![types],
            names: vec![None],
            needs: vec![Vec::new()],
            grown: vec![RefCell::default()],
            indexes: RefCell::default(),
        }
    }

    /// The value of a column in a tuple.
    pub(crate) fn var(&self, tuple: &[usize], var: Var) -> Value {
        let row = tuple.get(var.relation).copied().unwrap_or(NONE);
        if row == NONE {
            return Value::Null;
        }
        if var.attnum == TABLE_OID_ATTNUM {
            // A row of a partitioned table, or of a table with children of `INHERITS`, has the OID of its table after the ID of its version.
            let width = self.types[var.relation].len();
            let leaf = if self.needs[var.relation].is_empty() {
                self.rows[var.relation][row].get(width + 1).cloned()
            } else {
                self.grown[var.relation].borrow()[row].get(width + 1).cloned()
            };
            if let Some(Value::Oid(leaf)) = leaf {
                return Value::Oid(leaf);
            }
            return Value::Oid(self.oids[var.relation]);
        }
        // rupg has no row versions yet, so each row shows `xmin`, `cmin`, `xmax` and `cmax` of a frozen row that no transaction deleted.
        if var.attnum == XMIN_ATTNUM {
            return Value::Oid(FROZEN_TRANSACTION_ID);
        }
        let width = self.types[var.relation].len();
        if var.attnum == CTID_ATTNUM {
            // A row of a table of the user ends with the ID of its version, which `UPDATE` and `DELETE` read as `ctid`.
            let tid = if self.needs[var.relation].is_empty() {
                self.rows[var.relation][row].get(width).cloned()
            } else {
                self.grown[var.relation].borrow()[row].get(width).cloned()
            };
            if let Some(tid) = tid {
                return tid;
            }
        }
        if var.attnum < 0 {
            return Value::Oid(0);
        }
        if var.attnum == 0 {
            let mut values = if self.needs[var.relation].is_empty() {
                self.rows[var.relation][row].clone()
            } else {
                self.grown[var.relation].borrow()[row].clone()
            };
            values.truncate(width);
            let mut types = self.types[var.relation].clone();
            let mut names = self.names[var.relation].clone();
            // A dropped column of a table has the type 0, and the row type has no field for it.
            if self.oids[var.relation] != 0 && types.contains(&0) {
                let live = |i: &usize| types[*i] != 0;
                values = (0..values.len()).filter(live).map(|i| values[i].clone()).collect();
                names = names.map(|n| (0..n.len()).filter(live).map(|i| n[i].clone()).collect());
                types.retain(|&t| t != 0);
            }
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

    /// The number of the rows that each relation that reads other relations has now, which [`Tables::compact`] takes later.
    fn marks(&self) -> Vec<usize> {
        self.grown.iter().map(|g| g.borrow().len()).collect()
    }

    /// Frees the rows that the runs added after `marks` and that no tuple of `tuples` uses, and gives the tuples the new row numbers. A nested loop runs a relation that reads other relations once for each outer row, and most of the rows of a run join no row. Without this, the rows of all the runs stay until the query ends.
    fn compact(&self, marks: &[usize], tuples: &mut [Vec<usize>]) {
        for (relation, &mark) in marks.iter().enumerate() {
            let mut grown = self.grown[relation].borrow_mut();
            if grown.len() == mark {
                continue;
            }
            let mut used: Vec<usize> = tuples
                .iter()
                .map(|t| t[relation])
                .filter(|&row| row != NONE && row >= mark)
                .collect();
            used.sort_unstable();
            used.dedup();
            let mut tail = grown.split_off(mark);
            grown.extend(used.iter().map(|&row| std::mem::take(&mut tail[row - mark])));
            for tuple in tuples.iter_mut() {
                let row = tuple[relation];
                if row != NONE
                    && row >= mark
                    && let Ok(at) = used.binary_search(&row)
                {
                    tuple[relation] = mark + at;
                }
            }
        }
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

    /// True when a relation of the part `relations` reads a relation of `left`.
    fn depends_on(&self, relations: &[usize], left: &[usize]) -> bool {
        relations.iter().any(|r| self.needs[*r].iter().any(|n| left.contains(n)))
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
        let work = relation
            .cte
            .as_ref()
            .is_some_and(|c| c.work.is_some() || c.write.is_some() || c.transition);
        if relation.subquery.is_some()
            || relation.function.is_some()
            || relation.values.is_some()
            || work
        {
            rows.push(Rc::default());
            continue;
        }
        let Some(catalog) = rupg_pgcatalog::catalog_by_oid(relation.oid) else {
            rows.push(Rc::new(user_rows(relation, session)?));
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

/// The rows of a query whose relations read the rows that the write gives, one list for each relation: the rows that the statement wrote for a query of `RETURNING`, or the old row and the row of `EXCLUDED` for the query of `ON CONFLICT DO UPDATE`.
pub(crate) fn given(query: &Query, rows: Vec<Vec<Vec<Value>>>) -> Tables {
    Tables {
        rows: rows.into_iter().map(Rc::new).collect(),
        oids: query.relations.iter().map(|r| r.oid).collect(),
        types: query.relations.iter().map(|r| r.columns.iter().map(|c| c.ty).collect()).collect(),
        names: query.relations.iter().map(row_names).collect(),
        needs: query.relations.iter().map(|_| Vec::new()).collect(),
        grown: query.relations.iter().map(|_| RefCell::default()).collect(),
        indexes: RefCell::default(),
    }
}

/// The names of the fields of a whole row of a relation. A subquery, a function or a `VALUES` list takes the names of the alias, then the names of its columns. A table takes the names of the columns of its row type, as the alias does not change the row type.
fn row_names(relation: &rupg_analyze::Relation) -> Option<Arc<[String]>> {
    let alias = if relation.oid == 0 { relation.alias.as_deref().unwrap_or_default() } else { &[] };
    let names = relation.columns.iter().enumerate();
    Some(names.map(|(i, c)| alias.get(i).unwrap_or(&c.name).clone()).collect())
}

/// The rows of a relation of the user. A table gives the rows that the snapshot of the statement sees, each with the ID of its version at the end. A partitioned table gives the rows of its leaf partitions in the order of their bounds, each with the OID of its partition after the ID of its version, or no rows after `ONLY`. Without `ONLY`, a table with children of `INHERITS` gives its rows and then the rows of the children, each with the OID of its table after the ID of its version. A sequence has its one row, the row of its file. The read of a table counts as a sequential scan, because rupg has no index scans yet.
fn user_rows(from: &rupg_analyze::Relation, session: &dyn Session) -> Result<Vec<Vec<Value>>> {
    let oid = from.oid;
    let relation = session
        .catalog()
        .and_then(|c| c.relation(oid))
        .ok_or_else(|| Error::internal(format!("no relation has the OID {oid}")))?;
    if relation.kind == rupg_catalog::RelKind::Partitioned {
        if from.only {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for leaf in crate::partition::leaves(oid, session)? {
            child_rows(relation, leaf, session, &mut out)?;
        }
        return Ok(out);
    }
    if relation.kind == rupg_catalog::RelKind::Table {
        session.count_scan(oid);
        // A row from before an `ALTER TABLE ... ADD COLUMN` has no value for the new columns, which are null. The ID of the version stays at the end of the row.
        let width = relation.columns.len();
        let mut rows = session.table_rows(oid)?;
        let children = match session.catalog() {
            Some(catalog) if !from.only => catalog.all_inheritors(oid),
            _ => Vec::new(),
        };
        for row in &mut rows {
            let tid = row.pop().unwrap_or(Value::Null);
            row.resize(width, Value::Null);
            row.push(tid);
            if !children.is_empty() {
                row.push(Value::Oid(oid));
            }
        }
        // `expand_inherited_rtentry`: the rows of the tables that inherit from the table come after its own rows.
        for child in children {
            child_rows(relation, child, session, &mut rows)?;
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

/// The rows of the table `child`, a leaf partition of the partitioned table `relation` or a table that inherits from the table `relation`, with the columns of `relation`. Each row has the ID of its version and then the OID of `child` at the end.
fn child_rows(
    relation: &rupg_catalog::Relation,
    child: u32,
    session: &dyn Session,
    out: &mut Vec<Vec<Value>>,
) -> Result<()> {
    let Some(part) = session.catalog().and_then(|c| c.relation(child)) else { return Ok(()) };
    // `build_attrmap_by_name`: the column of the child for each column of the parent.
    let map: Vec<Option<usize>> = relation
        .columns
        .iter()
        .map(|c| {
            let found = part.columns.iter().position(|p| !p.dropped && p.name == c.name);
            found.filter(|_| !c.dropped)
        })
        .collect();
    session.count_scan(child);
    for mut row in session.table_rows(child)? {
        let tid = row.pop().unwrap_or(Value::Null);
        let mut values: Vec<Value> = map
            .iter()
            .map(|i| i.and_then(|i| row.get(i).cloned()).unwrap_or(Value::Null))
            .collect();
        values.push(tid);
        values.push(Value::Oid(child));
        out.push(values);
    }
    Ok(())
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

/// True when an expression reads columns of an outer query and nothing of its own query, so that it has one value for each run of the query: a column of an outer query, a constant, a parameter, or a function that is not volatile of such expressions, as `t1.tenthous + 1`. An index of the other side then gives the rows of each run, as an index scan of PostgreSQL does with a parameter of the outer plan.
fn outer_expr(expr: &Expr) -> bool {
    fn fixed(expr: &Expr, outer: &mut bool) -> bool {
        match &expr.kind {
            ExprKind::Var(var) if var.levels_up > 0 => {
                *outer = true;
                true
            }
            ExprKind::Const(_) | ExprKind::Param(_) => true,
            ExprKind::Relabel(arg, _) | ExprKind::CoerceViaIo(arg, _) => fixed(arg, outer),
            ExprKind::Func(f) if !f.retset && !crate::volatile(expr) => {
                f.args.iter().all(|arg| fixed(arg, outer))
            }
            _ => false,
        }
    }
    let mut outer = false;
    fixed(expr, &mut outer) && outer
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
        // A part that reads only relations outside the join, which `context` holds, runs once for all the tuples of the left side.
        if !self.tables.depends_on(input.right_relations, input.left_relations) {
            input.right = self.item_tuples(item, conds, context)?;
            return join(input, self.tables, self.test);
        }
        let left = std::mem::take(&mut input.left);
        let limit = join_limit(self.width);
        let mut out = Vec::new();
        for l in left {
            let context = merged(context, &l);
            let marks = self.tables.marks();
            let right = self.item_tuples(item, conds.clone(), &context)?;
            let input = JoinInput { left: vec![l], right, ..input };
            let mut tuples = join(input, self.tables, self.test)?;
            self.tables.compact(&marks, &mut tuples);
            if out.len() + tuples.len() > limit {
                return Err(too_many(limit));
            }
            out.extend(tuples);
        }
        Ok(out)
    }

    /// The rows of a relation that can pass the conditions `conds`, through an index of a column, when a condition is an `=` of the column and an expression of the columns of an outer query, as in a correlated subquery. The subquery runs for each row of the outer query, and the index makes each run read only the rows with the value of that row, as an index scan with a parameter does. `None` when no condition is such an `=`.
    fn lookup(&mut self, relation: usize, conds: &[&Expr]) -> Result<Option<Vec<usize>>> {
        let found = conds.iter().find_map(|expr| {
            let ExprKind::Func(f) = &expr.kind else { return None };
            if !HASH_EQUALS.contains(&f.oid) || f.args.len() != 2 {
                return None;
            }
            match (plain_var(&f.args[0]), plain_var(&f.args[1])) {
                (Some(var), None) if var.relation == relation && outer_expr(&f.args[1]) => {
                    Some((var, &f.args[1]))
                }
                (None, Some(var)) if var.relation == relation && outer_expr(&f.args[0]) => {
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
    let width = left.first().or(right.first()).map_or(0, Vec::len);
    let limit = join_limit(width);
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
                if out.len() == limit {
                    return Err(too_many(limit));
                }
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
