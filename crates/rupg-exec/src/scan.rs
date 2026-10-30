//! The scan of the relations and the joins of `FROM`. A table of the catalog gives its static rows, then the rows of the user objects of the session. A relation of the user gives the rows of [`user_rows`]. The rows of a subquery or a function in `FROM` come from the executor, which adds them to [`Tables`]. A subquery or a function that reads the relations before it, as `LATERAL` lets it, runs in the scan for each row of those relations.
//!
//! A tuple of the join is the row number of each relation of the query, or [`NONE`] for a relation that a row of an outer join does not have. The rows of each relation are read once, with only the columns that the query uses.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use rupg_analyze::{
    Expr, ExprKind, FromFunction, FromItem, JoinKind, Query, TABLE_OID_ATTNUM, Var,
};
use rupg_common::{Error, Result, SqlState};
use rupg_func::Session;
use rupg_pgcatalog::{Batch, Values};
use rupg_plan::Scanned;
use rupg_types::{Array, Record, Value, oid};

/// The attribute number of the system column `xmin`.
const XMIN_ATTNUM: i16 = -2;

/// `SelfItemPointerAttributeNumber`: the attribute number of `ctid`.
const CTID_ATTNUM: i16 = -1;

/// The bits of the ID of a version that are its item pointer, `(page << 16) | item`. The bits above them are the generation of the heap of the table.
const ITEM_POINTER_MASK: i64 = (1 << 48) - 1;

/// `heap_getsysattr` for `ctid`: a column `ctid` of the type `tid` gives the item pointer of the ID of the version of the row, `(page,item)`, as `tidout` shows it. The plans of a write and of a cursor read the ID itself, as a `bigint`.
pub(crate) fn item_pointer(value: Value, var: Var, ty: u32) -> Value {
    match value {
        Value::Int8(id) if var.attnum == CTID_ATTNUM && ty == oid::TID => {
            Value::Int8(id & ITEM_POINTER_MASK)
        }
        value => value,
    }
}

/// `MaxTransactionIdAttributeNumber`: the attribute number of `xmax`.
const XMAX_ATTNUM: i16 = -4;

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
    /// For each relation, true when its rows end with the OID of the table of the row and the system columns `xmin` and `xmax` after the ID of the version, because the query reads `xmin` or `xmax`.
    xids: Vec<bool>,
    /// The transaction that wrote the rows that `RETURNING` reads, which is `xmin` of each row.
    writer: Option<u32>,
    /// The rows of each relation that reads other relations. The scan adds the rows of each run.
    grown: Vec<RefCell<Vec<Vec<Value>>>>,
    /// The index of a column, by the relation and the attribute number, that the scan makes for the first run of a correlated subquery and uses again in the next runs.
    indexes: RefCell<Indexes>,
    /// For each relation whose scan kept only some columns, the place of each column in the rows. A relation with no entry has all the columns in each row.
    places: Vec<Option<Places>>,
}

/// The columns that the rows of a scan keep: the place of each column of the relation in a row, or `NONE` for a column that the rows do not have, and the number of the kept columns, after which the ID of the version and the other system values come.
#[derive(Clone, Debug)]
pub(crate) struct Places {
    at: Rc<[usize]>,
    width: usize,
}

impl Places {
    /// The places of the columns that are true in `kept`.
    fn new(kept: &[bool]) -> Places {
        let mut width = 0;
        let at = kept
            .iter()
            .map(|&k| {
                if !k {
                    return NONE;
                }
                width += 1;
                width - 1
            })
            .collect();
        Places { at, width }
    }
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
            xids: vec![false],
            writer: None,
            grown: vec![RefCell::default()],
            indexes: RefCell::default(),
            places: Vec::new(),
        }
    }

    /// The number of the columns in each row of a relation, after which the ID of the version comes.
    fn width(&self, relation: usize) -> usize {
        match self.places.get(relation) {
            Some(Some(places)) => places.width,
            _ => self.types[relation].len(),
        }
    }

    /// The place of the column `column` of a relation in its rows, or `None` for a column that the rows do not have.
    pub(crate) fn place(&self, relation: usize, column: usize) -> Option<usize> {
        match self.places.get(relation) {
            Some(Some(places)) => places.at.get(column).copied().filter(|&at| at != NONE),
            _ => Some(column),
        }
    }

    /// The types of the columns that the rows of a relation have, in their places.
    fn stored_types(&self, relation: usize) -> Vec<u32> {
        let types = &self.types[relation];
        (0..types.len()).filter(|&c| self.place(relation, c).is_some()).map(|c| types[c]).collect()
    }

    /// The value of a column in a tuple.
    pub(crate) fn var(&self, tuple: &[usize], var: Var) -> Value {
        let row = tuple.get(var.relation).copied().unwrap_or(NONE);
        if row == NONE {
            return Value::Null;
        }
        let width = self.width(var.relation);
        if var.attnum == TABLE_OID_ATTNUM {
            // A row of a partitioned table, or of a table with children of `INHERITS`, has the OID of its table after the ID of its version.
            if let Some(Value::Oid(leaf)) = self.trailer(var.relation, row, width + 1) {
                return Value::Oid(leaf);
            }
            return Value::Oid(self.oids[var.relation]);
        }
        if matches!(var.attnum, XMIN_ATTNUM | XMAX_ATTNUM) {
            // A row of the catalog shows `xmin` and `xmax` of a frozen row that no transaction deleted.
            let at = if var.attnum == XMIN_ATTNUM { width + 2 } else { width + 3 };
            let xid = self.xids.get(var.relation).copied().unwrap_or(false);
            if let Some(value) = self.trailer(var.relation, row, at).filter(|_| xid) {
                return value;
            }
            if let Some(writer) = self.writer {
                return Value::Oid(if var.attnum == XMIN_ATTNUM { writer } else { 0 });
            }
            let frozen = if var.attnum == XMIN_ATTNUM { FROZEN_TRANSACTION_ID } else { 0 };
            return Value::Oid(frozen);
        }
        if var.attnum == CTID_ATTNUM {
            // A row of a table of the user ends with the ID of its version, which `UPDATE` and `DELETE` read as `ctid`.
            if let Some(tid) = self.trailer(var.relation, row, width) {
                return tid;
            }
            // A row of the catalog has no version. Its `ctid` is its place in the catalog, with 32768 rows on a page, so that each row has a different `ctid`.
            if self.oids[var.relation] < rupg_catalog::FIRST_NORMAL_OID {
                let row = i64::try_from(row).unwrap_or_default();
                return Value::Int8(((row >> 15) << 16) | ((row & 0x7fff) + 1));
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
            if self.width(var.relation) != self.types[var.relation].len() {
                let stored = values;
                let columns = 0..self.types[var.relation].len();
                let at = |c| self.place(var.relation, c).and_then(|at| stored.get(at).cloned());
                values = columns.map(|c| at(c).unwrap_or(Value::Null)).collect();
            }
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
        let Some(column) = self.place(var.relation, column) else { return Value::Null };
        if self.needs[var.relation].is_empty() {
            return self.rows[var.relation][row].get(column).cloned().unwrap_or(Value::Null);
        }
        let grown = self.grown[var.relation].borrow();
        grown[row].get(column).cloned().unwrap_or(Value::Null)
    }

    /// The OID of the table of a row of a relation: the partition or the child of `INHERITS` of the row, or the table of the relation.
    pub(crate) fn leaf(&self, relation: usize, row: usize) -> u32 {
        let width = self.width(relation);
        match self.trailer(relation, row, width + 1) {
            Some(Value::Oid(leaf)) => leaf,
            _ => self.oids[relation],
        }
    }

    /// The heap page of a row of a relation, from the ID of its version, or `None` for a row with no version.
    pub(crate) fn page(&self, relation: usize, row: usize) -> Option<i64> {
        let width = self.width(relation);
        match self.trailer(relation, row, width) {
            Some(Value::Int8(id)) => Some((id & ITEM_POINTER_MASK) >> 16),
            _ => None,
        }
    }

    /// The tables of [`given`] for the rows that the transaction `writer` inserted, whose `xmin` is `writer` and whose `xmax` is 0.
    pub(crate) fn written_by(self, writer: Option<u32>) -> Tables {
        Tables { writer, ..self }
    }

    /// The value at the place `at` of the row `row` of the relation `relation`, after the columns, or `None` when the row is shorter.
    fn trailer(&self, relation: usize, row: usize, at: usize) -> Option<Value> {
        if self.needs[relation].is_empty() {
            self.rows[relation][row].get(at).cloned()
        } else {
            self.grown[relation].borrow()[row].get(at).cloned()
        }
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

/// Adds to `out` the relations of a part of `FROM` that an outer join can give as nulls.
fn nullable(item: &FromItem, out: &mut Vec<usize>) {
    if let FromItem::Join(join) = item {
        nullable(&join.left, out);
        nullable(&join.right, out);
        if matches!(join.kind, JoinKind::Left | JoinKind::Full) {
            relations_in(&join.right, out);
        }
        if matches!(join.kind, JoinKind::Right | JoinKind::Full) {
            relations_in(&join.left, out);
        }
    }
}

/// The relations outside a part of `FROM` that the conditions of its joins read, as the conditions of a `LATERAL` subquery that the planner pulled up do.
fn join_reads(item: &FromItem) -> Vec<usize> {
    fn conditions(item: &FromItem, out: &mut Vec<usize>) {
        if let FromItem::Join(join) = item {
            if let Some(on) = &join.on {
                out.extend(relations_of(on));
            }
            conditions(&join.left, out);
            conditions(&join.right, out);
        }
    }
    let (mut read, mut own) = (Vec::new(), Vec::new());
    conditions(item, &mut read);
    relations_in(item, &mut own);
    read.retain(|r| !own.contains(r));
    read.sort_unstable();
    read.dedup();
    read
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

/// The tables that a scan of the relation `index` of the query reads, as `expand_inherited_rtentry` and `prune_append_rel_partitions` choose them: the leaf partitions of a partitioned table that the conditions `conds` of the scan do not prune, or a table with children of `INHERITS` and its children. `None` for a relation that is one table.
///
/// # Errors
///
/// The errors of the comparisons of the bounds of the partitions.
pub fn members(
    query: &Query,
    index: usize,
    conds: &[&Expr],
    session: &dyn Session,
) -> Result<Option<Vec<u32>>> {
    let Some(from) = query.relations.get(index) else { return Ok(None) };
    if from.subquery.is_some()
        || from.function.is_some()
        || from.values.is_some()
        || from.cte.is_some()
        || rupg_pgcatalog::catalog_by_oid(from.oid).is_some()
    {
        return Ok(None);
    }
    let Some(relation) = session.catalog().and_then(|c| c.relation(from.oid)) else {
        return Ok(None);
    };
    if relation.kind == rupg_catalog::RelKind::Partitioned {
        if from.only {
            return Ok(Some(Vec::new()));
        }
        return crate::partition::pruned_leaves(from.oid, index, conds, false, session).map(Some);
    }
    if from.only || relation.kind != rupg_catalog::RelKind::Table {
        return Ok(None);
    }
    let children = session.catalog().map(|c| c.all_inheritors(from.oid)).unwrap_or_default();
    if children.is_empty() {
        return Ok(None);
    }
    Ok(Some(std::iter::once(from.oid).chain(children).collect()))
}

/// The value of a condition of `WHERE` for a tuple of rows of some [`Tables`].
pub(crate) type Narrow<'a> = dyn FnMut(&Tables, &Expr, &[usize]) -> Result<Value> + 'a;

/// Reads the rows of the relations of a query. With `narrow`, which gives the value of a condition, a query of one table whose conditions of `WHERE` read only some of the columns that the query reads first reads only the columns of the conditions, and then the other columns of the rows that pass all the conditions, so that a scan that keeps few rows of a wide table copies only those rows.
///
/// # Errors
///
/// The error of the input function of a column that the catalog keeps as text, and the error of a condition.
pub(crate) fn read(
    query: &Query,
    session: &dyn Session,
    mut narrow: Option<&mut Narrow<'_>>,
) -> Result<Tables> {
    let mut used: Vec<Vec<bool>> =
        query.relations.iter().map(|r| vec![false; r.columns.len()]).collect();
    let mut xids = vec![false; query.relations.len()];
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
                    Err(_) if matches!(var.attnum, XMIN_ATTNUM | XMAX_ATTNUM) => {
                        xids[var.relation] = true;
                    }
                    Err(_) => {}
                }
            }
            None::<()>
        });
    }
    let mut conds = Vec::new();
    if let Some(filter) = &query.filter {
        conjuncts(filter, &mut conds);
    }
    let mut nulls = Vec::new();
    for item in &query.from {
        nullable(item, &mut nulls);
    }
    // A write and a row lock read the whole rows of their tables, so only a plain query copies just the columns that it reads.
    let plain = query.write.is_none() && query.row_marks.is_empty();
    let mut rows = Vec::with_capacity(query.relations.len());
    let mut places = vec![None; query.relations.len()];
    for (index, (relation, used)) in query.relations.iter().zip(&used).enumerate() {
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
            let nullable = nulls.contains(&index);
            let from = (index, nullable, xids[index]);
            let kept = plain.then(|| kept_columns(relation.oid, used, session));
            // The rows keep only the columns that the query reads, so that a scan of a few columns of a wide table is small.
            let kept = kept.filter(|k| k.contains(&false));
            places[index] = kept.as_deref().map(Places::new);
            let few = narrow.as_mut().filter(|_| !xids[index]).and_then(|test| {
                let mask = narrowed(query, kept.as_deref().unwrap_or(used), &conds, session)?;
                Some((test, mask))
            });
            if let Some((test, mask)) = few {
                let few = user_rows(relation, from, Some(&mask), &conds, session)?;
                let mut few =
                    tables(query, vec![Rc::new(few)], xids.clone(), vec![Some(Places::new(&mask))]);
                let mut tids = Vec::new();
                'row: for row in 0..few.rows[index].len() {
                    for expr in &conds {
                        if test(&few, expr, &[row])? != Value::Bool(true) {
                            continue 'row;
                        }
                    }
                    if let Some(Value::Int8(tid)) = few.rows[index][row].last() {
                        tids.push(tid.cast_unsigned());
                    }
                }
                few.rows.clear();
                let width = kept
                    .as_ref()
                    .map_or(relation.columns.len(), |k| k.iter().filter(|k| **k).count());
                let mut kept_rows = session.table_rows_at(relation.oid, kept.as_deref(), &tids)?;
                for row in &mut kept_rows {
                    let tid = row.pop().unwrap_or(Value::Null);
                    row.resize(width, Value::Null);
                    row.push(tid);
                }
                rows.push(Rc::new(kept_rows));
                continue;
            }
            rows.push(Rc::new(user_rows(relation, from, kept.as_deref(), &conds, session)?));
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
        if let Some(user) = session.system_catalog() {
            crate::user::grant_static(catalog, &mut table, user, session)?;
            crate::user::drop_static(catalog, &mut table, user, session)?;
            table.extend(crate::user::rows(catalog, user, session)?);
        }
        rows.push(Rc::new(table));
    }
    Ok(tables(query, rows, xids, places))
}

/// The [`Tables`] of a query with the rows of its relations.
fn tables(
    query: &Query,
    rows: Vec<Rc<Vec<Vec<Value>>>>,
    xids: Vec<bool>,
    places: Vec<Option<Places>>,
) -> Tables {
    Tables {
        rows,
        oids: query.relations.iter().map(|r| r.oid).collect(),
        types: query.relations.iter().map(|r| r.columns.iter().map(|c| c.ty).collect()).collect(),
        names: query.relations.iter().map(row_names).collect(),
        needs: lateral(query),
        xids,
        writer: None,
        grown: query.relations.iter().map(|_| RefCell::default()).collect(),
        indexes: RefCell::default(),
        places,
    }
}

/// The columns that the conditions `conds` of `WHERE` read, for a scan that tests them before it reads the other columns `kept` of the rows, which [`read`] does. The query must read one table of the user with no children and no sample, through no write and no row lock, and each condition must read only the columns of the table with the built-in functions that are not volatile, with no `=` that [`Scan::lookup`] can test through a hash. An `=` lets the scan test the other conditions on fewer rows, and the conditions run again on the rows that pass, so a condition that is not volatile gives the same value. The conditions must read fewer columns than the query.
fn narrowed(
    query: &Query,
    kept: &[bool],
    conds: &[&Expr],
    session: &dyn Session,
) -> Option<Vec<bool>> {
    let [relation] = query.relations.as_slice() else { return None };
    if conds.is_empty()
        || !matches!(query.from.as_slice(), [FromItem::Relation(0)])
        || relation.tablesample.is_some()
        || relation.cte.is_some()
        || query.write.is_some()
        || !query.row_marks.is_empty()
    {
        return None;
    }
    let catalog = session.catalog()?;
    let table = catalog.relation(relation.oid)?;
    if table.kind != rupg_catalog::RelKind::Table
        || !catalog.all_inheritors(relation.oid).is_empty()
    {
        return None;
    }
    let builtin = |func: u32| {
        func < rupg_catalog::FIRST_NORMAL_OID
            && rupg_pgcatalog::builtin::proc_by_oid(func).is_some_and(|p| p.volatile != b'v')
    };
    let mut mask = vec![false; kept.len()];
    for expr in conds {
        if let ExprKind::Func(f) = &expr.kind
            && HASH_EQUALS.contains(&f.oid)
        {
            return None;
        }
        let other = expr.find(0, &mut |e, depth| match &e.kind {
            ExprKind::Var(var) if var.levels_up == depth && var.relation == 0 => {
                match usize::try_from(var.attnum - 1).ok().and_then(|i| mask.get_mut(i)) {
                    Some(slot) => {
                        *slot = true;
                        None
                    }
                    None => Some(()),
                }
            }
            ExprKind::Func(f) if builtin(f.oid) => None,
            ExprKind::NullIf { equal: func, .. }
            | ExprKind::Distinct { equal: func, .. }
            | ExprKind::ScalarArrayOp { func, .. }
                if builtin(*func) =>
            {
                None
            }
            ExprKind::Const(_)
            | ExprKind::Relabel(..)
            | ExprKind::Collate(..)
            | ExprKind::Bool(..)
            | ExprKind::NullTest(..)
            | ExprKind::BooleanTest(..)
            | ExprKind::Coalesce(_)
            | ExprKind::Array { .. }
            | ExprKind::Case(_)
            | ExprKind::CaseTest => None,
            _ => Some(()),
        });
        if other.is_some() {
            return None;
        }
    }
    let fewer = mask.iter().zip(kept).all(|(m, k)| !m || *k)
        && mask.iter().filter(|m| **m).count() < kept.iter().filter(|k| **k).count();
    fewer.then_some(mask)
}

/// The rows of a query whose relations read the rows that the write gives, one list for each relation: the rows that the statement wrote for a query of `RETURNING`, or the old row and the row of `EXCLUDED` for the query of `ON CONFLICT DO UPDATE`.
pub(crate) fn given(query: &Query, rows: Vec<Vec<Vec<Value>>>) -> Tables {
    Tables {
        rows: rows.into_iter().map(Rc::new).collect(),
        oids: query.relations.iter().map(|r| r.oid).collect(),
        types: query.relations.iter().map(|r| r.columns.iter().map(|c| c.ty).collect()).collect(),
        names: query.relations.iter().map(row_names).collect(),
        needs: query.relations.iter().map(|_| Vec::new()).collect(),
        xids: vec![false; query.relations.len()],
        writer: None,
        grown: query.relations.iter().map(|_| RefCell::default()).collect(),
        indexes: RefCell::default(),
        places: Vec::new(),
    }
}

/// The columns that a scan of the table `oid` copies for a query that reads the columns `used`: these columns and the plain key columns of each index of the table, because an index scan gives the rows in the order of all the keys of its index.
fn kept_columns(oid: u32, used: &[bool], session: &dyn Session) -> Vec<bool> {
    let mut kept = used.to_vec();
    let Some(catalog) = session.catalog() else { return kept };
    for index in catalog.relations().filter_map(|r| r.index.as_ref().filter(|i| i.table == oid)) {
        for &key in &index.keys {
            if let Some(slot) = usize::try_from(key - 1).ok().and_then(|i| kept.get_mut(i)) {
                *slot = true;
            }
        }
    }
    kept
}

/// The names of the fields of a whole row of a relation. A subquery, a function or a `VALUES` list takes the names of the alias, then the names of its columns. A table takes the names of the columns of its row type, as the alias does not change the row type.
fn row_names(relation: &rupg_analyze::Relation) -> Option<Arc<[String]>> {
    let alias = if relation.oid == 0 { relation.alias.as_deref().unwrap_or_default() } else { &[] };
    let names = relation.columns.iter().enumerate();
    Some(names.map(|(i, c)| alias.get(i).unwrap_or(&c.name).clone()).collect())
}

/// The rows of a relation of the user. A table gives the rows that the snapshot of the statement sees, each with the ID of its version at the end. A partitioned table gives the rows of its leaf partitions in the order of their bounds, each with the OID of its partition after the ID of its version, or no rows after `ONLY`. Without `ONLY`, a table with children of `INHERITS` gives its rows and then the rows of the children, each with the OID of its table after the ID of its version. A sequence has its one row, the row of its file. The read of a table counts as a sequential scan, because rupg has no index scans yet.
/// The error of a scan of a materialized view of `WITH NO DATA`, as `ExecCheckRelationsScannable` gives it.
fn check_populated(relation: &rupg_catalog::Relation) -> Result<()> {
    if relation.kind == rupg_catalog::RelKind::MatView
        && relation.view.as_ref().is_some_and(|v| !v.populated)
    {
        return Err(Error::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("materialized view \"{}\" has not been populated", relation.name),
        )
        .with_hint("Use the REFRESH MATERIALIZED VIEW command."));
    }
    Ok(())
}

/// `GetFdwRoutineByRelId` for a foreign table: the foreign-data wrapper of its server must have a handler. rupg has no handler, so a scan or a write of a foreign table gives an error.
fn check_handler(relation: &rupg_catalog::Relation, session: &dyn Session) -> Result<()> {
    let Some(foreign) = &relation.foreign else { return Ok(()) };
    let catalog = session.catalog();
    let server = catalog.and_then(|c| c.server(foreign.server));
    let Some(fdw) = server.and_then(|s| catalog.and_then(|c| c.fdw(s.fdw))) else {
        return Err(Error::internal(format!(
            "cache lookup failed for foreign server {}",
            foreign.server
        )));
    };
    if fdw.handler == 0 {
        return Err(Error::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("foreign-data wrapper \"{}\" has no handler", fdw.name),
        ));
    }
    Err(Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "a scan of a foreign table is not supported yet",
    ))
}

/// The check of `get_relation_info` and `make_modifytable` of the planner: each foreign table that the query reads or writes, also in its subqueries, its queries of `WITH` and its sublinks, has a handler, as [`check_handler`] checks it.
pub(crate) fn check_foreign(query: &Query, session: &dyn Session) -> Result<()> {
    let Some(catalog) = session.catalog() else { return Ok(()) };
    if let Some(rel) = query.write.as_ref().and_then(|w| catalog.relation(w.table)) {
        check_handler(rel, session)?;
    }
    for relation in &query.relations {
        if let Some(sub) = relation.subquery.as_deref() {
            check_foreign(sub, session)?;
        } else if let Some(rel) = catalog.relation(relation.oid) {
            check_handler(rel, session)?;
        }
    }
    for cte in &query.ctes {
        check_foreign(&cte.query, session)?;
    }
    for expr in query.exprs() {
        let error = expr.find(0, &mut |e, _| match &e.kind {
            ExprKind::SubLink(sub) => check_foreign(&sub.query, session).err(),
            _ => None,
        });
        if let Some(error) = error {
            return Err(error);
        }
    }
    Ok(())
}

/// `ExecCheckRelationsScannable`: the materialized views that the query reads, also in its subqueries, its queries of `WITH` and its sublinks, are populated. The executor checks them when it starts, before the description of the rows.
pub(crate) fn check_scannable(query: &Query, session: &dyn Session) -> Result<()> {
    for relation in &query.relations {
        if let Some(sub) = relation.subquery.as_deref() {
            check_scannable(sub, session)?;
        } else if let Some(rel) = session.catalog().and_then(|c| c.relation(relation.oid)) {
            check_populated(rel)?;
        }
    }
    for cte in &query.ctes {
        check_scannable(&cte.query, session)?;
    }
    for expr in query.exprs() {
        let error = expr.find(0, &mut |e, _| match &e.kind {
            ExprKind::SubLink(sub) => check_scannable(&sub.query, session).err(),
            _ => None,
        });
        if let Some(error) = error {
            return Err(error);
        }
    }
    Ok(())
}

/// The rows of the relation `from` of the user, which is the relation `index` of the query, with true when an outer join can give it as nulls and true when the query reads `xmin` or `xmax`. With `used`, each row has only the columns that are true in `used`, in their order. A partitioned table gives the rows of the leaf partitions that the conditions `conds` of `WHERE` do not prune. For `xmin` and `xmax`, each row ends with the ID of its version, the OID of its table, `xmin` and `xmax`.
fn user_rows(
    from: &rupg_analyze::Relation,
    (index, nullable, xids): (usize, bool, bool),
    used: Option<&[bool]>,
    conds: &[&Expr],
    session: &dyn Session,
) -> Result<Vec<Vec<Value>>> {
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
        for leaf in crate::partition::pruned_leaves(oid, index, conds, nullable, session)? {
            child_rows(relation, (leaf, xids), used, session, &mut out)?;
        }
        return Ok(out);
    }
    check_populated(relation)?;
    if matches!(relation.kind, rupg_catalog::RelKind::Table | rupg_catalog::RelKind::MatView) {
        session.count_scan(oid);
        // A row from before an `ALTER TABLE ... ADD COLUMN` has no value for the new columns, which are null. The ID of the version stays at the end of the row.
        let width = used.map_or(relation.columns.len(), |u| u.iter().filter(|u| **u).count());
        let mut rows = if xids {
            session.table_rows_with_xids(oid, used)?
        } else {
            session.table_rows(oid, used)?
        };
        let children = match session.catalog() {
            Some(catalog) if !from.only => catalog.all_inheritors(oid),
            _ => Vec::new(),
        };
        for row in &mut rows {
            let tid = row.pop().unwrap_or(Value::Null);
            let system = if xids { row.split_off(row.len().saturating_sub(2)) } else { Vec::new() };
            row.resize(width, Value::Null);
            row.push(tid);
            if !children.is_empty() || xids {
                row.push(Value::Oid(oid));
            }
            row.extend(system);
        }
        // `expand_inherited_rtentry`: the rows of the tables that inherit from the table come after its own rows.
        for child in children {
            child_rows(relation, (child, xids), used, session, &mut rows)?;
        }
        return Ok(rows);
    }
    Ok(match &relation.sequence {
        Some(sequence) => {
            let row = session.sequence_row(sequence.file).unwrap_or(sequence.data);
            let values =
                [Value::Int8(row.last_value), Value::Int8(row.log_cnt), Value::Bool(row.is_called)];
            let kept = |i: usize| used.is_none_or(|u| u.get(i).copied().unwrap_or(false));
            vec![values.into_iter().enumerate().filter(|(i, _)| kept(*i)).map(|(_, v)| v).collect()]
        }
        None => Vec::new(),
    })
}

/// The rows of the table `child`, a leaf partition of the partitioned table `relation` or a table that inherits from the table `relation`, with the columns of `relation`. With `used`, each row has only the columns of `relation` that are true in `used`, in their order. Each row has the ID of its version and then the OID of `child` at the end, and then `xmin` and `xmax` when `xids` is true.
fn child_rows(
    relation: &rupg_catalog::Relation,
    (child, xids): (u32, bool),
    used: Option<&[bool]>,
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
    let kept = |i: usize| used.is_none_or(|u| u.get(i).copied().unwrap_or(false));
    // The columns of the child that the kept columns of `relation` map to, and the place of each column of the child in the rows of the child.
    let mask = used.map(|_| {
        let mut mask = vec![false; part.columns.len()];
        for (i, at) in map.iter().enumerate() {
            if let Some(at) = at.filter(|_| kept(i)) {
                mask[at] = true;
            }
        }
        mask
    });
    let places = mask.as_deref().map(Places::new);
    let place = |at: usize| match &places {
        Some(places) => places.at.get(at).copied().filter(|&at| at != NONE),
        None => Some(at),
    };
    let rows = if xids {
        session.table_rows_with_xids(child, mask.as_deref())?
    } else {
        session.table_rows(child, mask.as_deref())?
    };
    for mut row in rows {
        let tid = row.pop().unwrap_or(Value::Null);
        let system = if xids { row.split_off(row.len().saturating_sub(2)) } else { Vec::new() };
        let mut values: Vec<Value> = map
            .iter()
            .enumerate()
            .filter(|(i, _)| kept(*i))
            .map(|(_, i)| i.and_then(place).and_then(|at| row.get(at).cloned()))
            .map(|value| value.unwrap_or(Value::Null))
            .collect();
        values.push(tid);
        values.push(Value::Oid(child));
        values.extend(system);
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

/// True when an expression reads columns of an outer query or of the relations `bound` of the outer side of a nested loop, and nothing else of its own query, so that it has one value for each run of the scan: such a column, a constant, a parameter, or a function that is not volatile of such expressions, as `t1.tenthous + 1`. An index of the other side then gives the rows of each run, as an index scan of PostgreSQL does with a parameter of the outer plan.
fn outer_expr(expr: &Expr, bound: &[usize]) -> bool {
    fn fixed(expr: &Expr, bound: &[usize], outer: &mut bool) -> bool {
        match &expr.kind {
            ExprKind::Var(var) if var.levels_up > 0 || bound.contains(&var.relation) => {
                *outer = true;
                true
            }
            ExprKind::Const(_) | ExprKind::Param(_) => true,
            ExprKind::Relabel(arg, _) | ExprKind::CoerceViaIo(arg, _) => fixed(arg, bound, outer),
            ExprKind::Func(f) if !f.retset && !crate::volatile(expr) => {
                f.args.iter().all(|arg| fixed(arg, bound, outer))
            }
            _ => false,
        }
    }
    let mut outer = false;
    fixed(expr, bound, &mut outer) && outer
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

/// `order_qual_clauses`: the conditions in the order of their security levels, and in the order of their costs in each level. The first conditions are the conditions of the views with `security_barrier`, with the number of conditions of each view in `barrier`, and the other conditions come after them. A condition that is leakproof and costs less than 10 has the level 0, as it can give no information about the rows that a view hides. The sort is stable.
fn order_quals<'q>(filter: Vec<&'q Expr>, barrier: &[usize]) -> Vec<&'q Expr> {
    let mut levels =
        barrier.iter().enumerate().flat_map(|(level, &n)| std::iter::repeat_n(level, n));
    let mut items: Vec<(usize, f64, &Expr)> = filter
        .into_iter()
        .map(|expr| {
            let cost = qual_cost(expr);
            let level = levels.next().unwrap_or(barrier.len());
            let level = if cost < 10.0 && !expr.leaky() { 0 } else { level };
            (level, cost, expr)
        })
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    items.into_iter().map(|(_, _, expr)| expr).collect()
}

/// The cost of a condition for each row for `order_qual_clauses`, in units of `cpu_operator_cost`, as `cost_qual_eval` finds it: the costs of the calls of functions and operators, outside the subqueries, with the items of an array of `ANY` and `ALL`. A subquery that reads no column of the query runs once before the scan, as an `InitPlan` does, and costs nothing. The executor does not know the cost of the plan of a subquery that runs for each row, so it takes the cost of [`CORRELATED`].
fn qual_cost(expr: &Expr) -> f64 {
    let own = match &expr.kind {
        ExprKind::SubLink(link) if crate::correlated(&link.query) => CORRELATED,
        _ => rupg_opt::node_cost(expr),
    };
    own + expr.children().into_iter().map(qual_cost).sum::<f64>()
}

/// The cost of a subquery that runs for each row, for [`qual_cost`]: more than a call of a function in PL/pgSQL, which costs 100, as the scan of a small table costs more.
const CORRELATED: f64 = 10_000.0;

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
pub(crate) type Test<'a> = dyn FnMut(&Expr, &[usize]) -> Result<Value> + 'a;

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
    keep_in(tuples, conds, test, &[])
}

/// The tuples that pass every condition. A condition can read the relations of the outer side of a nested loop, whose rows `context` has.
fn keep_in(
    tuples: Vec<Vec<usize>>,
    conds: &[&Expr],
    test: &mut Test<'_>,
    context: &[usize],
) -> Result<Vec<Vec<usize>>> {
    if conds.is_empty() {
        return Ok(tuples);
    }
    let reads = context.iter().any(|&c| c != NONE)
        && conds.iter().any(|e| relations_of(e).iter().any(|&r| context[r] != NONE));
    let mut kept = Vec::with_capacity(tuples.len());
    'tuple: for tuple in tuples {
        let full = if reads { merged(context, &tuple) } else { Vec::new() };
        let seen = if reads { &full } else { &tuple };
        for expr in conds {
            if test(expr, seen)? != Value::Bool(true) {
                continue 'tuple;
            }
        }
        kept.push(tuple);
    }
    Ok(kept)
}

/// The relations that the outer side of a nested loop gives one row for each run of a part of `FROM`, which `context` has, other than the relations `own` of the part.
fn bound(context: &[usize], own: &[usize]) -> Vec<usize> {
    (0..context.len()).filter(|r| context[*r] != NONE && !own.contains(r)).collect()
}

/// The tuples of the query before the target list: the parts of `FROM` joined with a cross join, and the `WHERE` condition. Each part of `WHERE` that `AND` joins goes down to the smallest part of `FROM` that has its relations, where the query can test it on fewer rows. `test` gives true when a condition is true for a tuple. The tuples come in the order `planned` of the joins of the plan, which [`rupg_opt::join_order`] gives, when it has each relation of `FROM`. The rows of a relation of an index scan come in the order of the keys of the index, and the rows with the same keys in the order of the heap, as a `btree` index keeps them.
///
/// # Errors
///
/// The error of a condition.
pub(crate) fn tuples(
    query: &Query,
    tables: &Tables,
    test: &mut Test<'_>,
    source: &mut Source<'_>,
    planned: Option<(&[Scanned], &dyn Session)>,
) -> Result<Vec<Vec<usize>>> {
    let width = query.relations.len();
    let mut scan = Scan { tables, width, test, source };
    let mut filter = Vec::new();
    if let Some(expr) = &query.filter {
        conjuncts(expr, &mut filter);
    }
    // The one-time filter of the gate ran each condition that reads no column and calls no volatile function before the scan.
    filter.retain(|expr| expr.first_var().is_some() || crate::volatile(expr));
    let filter = order_quals(filter, &query.barrier);
    let items: Vec<(&FromItem, Vec<usize>)> = query
        .from
        .iter()
        .map(|item| {
            let mut relations = Vec::new();
            relations_in(item, &mut relations);
            (item, relations)
        })
        .collect();
    // A condition with no relation stays at the top, so that it runs once for each tuple as in a query with no FROM. With one item in `FROM`, it is a condition of the item, as `distribute_qual_to_rels` gives a volatile condition with no column the relations of its level, so that it runs with the other conditions of the scan in the order of their costs.
    let mut own: Vec<Vec<&Expr>> = vec![Vec::new(); items.len()];
    let mut rest = Vec::new();
    for expr in filter {
        let relations = relations_of(expr);
        if relations.is_empty() && items.len() == 1 {
            own[0].push(expr);
            continue;
        }
        match items.iter().position(|(_, r)| !relations.is_empty() && covered(expr, r)) {
            Some(i) => own[i].push(expr),
            None => rest.push(expr),
        }
    }
    let mut acc: Vec<Vec<usize>> = vec![vec![NONE; width]];
    let mut acc_relations: Vec<usize> = Vec::new();
    let order = join_order(&items, &rest, tables);
    let sequence = order.clone().unwrap_or_else(|| (0..items.len()).collect());
    for i in sequence {
        let (item, relations) = &items[i];
        let conds = std::mem::take(&mut own[i]);
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
    let mut acc = keep(acc, &rest, scan.test)?;
    let mut all: Vec<usize> = items.iter().flat_map(|(_, r)| r.iter().copied()).collect();
    all.sort_unstable();
    let planned = planned.filter(|(planned, _)| {
        let mut relations: Vec<usize> = planned.iter().map(|s| s.relation).collect();
        relations.sort_unstable();
        relations == all
    });
    if let Some((planned, session)) = planned {
        let mut ranks = Vec::with_capacity(planned.len());
        for s in planned {
            ranks.push(if s.keys.is_empty() {
                None
            } else {
                Some(index_ranks(tables, s, &acc, session)?)
            });
        }
        let cmp = |a: &Vec<usize>, b: &Vec<usize>| {
            planned
                .iter()
                .zip(&ranks)
                .map(|(s, rank)| {
                    let at = |row: usize| match rank {
                        Some(rank) => rank.get(&row).copied().unwrap_or(row),
                        None => row,
                    };
                    let (x, y) = (at(a[s.relation]), at(b[s.relation]));
                    if s.reverse { y.cmp(&x) } else { x.cmp(&y) }
                })
                .find(|o| o.is_ne())
                .unwrap_or(Ordering::Equal)
        };
        if !acc.is_sorted_by(|a, b| cmp(a, b).is_le()) {
            acc.sort_by(cmp);
        }
    } else if order.is_some() {
        // The tuples come in the order of the joins in the written order, as the row numbers of the relations of each part give it.
        let columns: Vec<usize> = items.iter().flat_map(|(_, r)| r.iter().copied()).collect();
        acc.sort_by(|a, b| columns.iter().map(|&c| a[c]).cmp(columns.iter().map(|&c| b[c])));
    }
    Ok(acc)
}

/// The place of each row of the relation of the index scan `scanned` in the tuples `acc`, in the order of the keys of the index and then of the heap, or the reverse order of the heap for a backward scan. A null row of an outer join keeps its number, after the others.
fn index_ranks(
    tables: &Tables,
    scanned: &Scanned,
    acc: &[Vec<usize>],
    session: &dyn Session,
) -> Result<BTreeMap<usize, usize>> {
    let r = scanned.relation;
    let rows = &tables.rows[r];
    let mut ids: Vec<usize> = acc.iter().map(|t| t[r]).filter(|&row| row < rows.len()).collect();
    ids.sort_unstable();
    ids.dedup();
    // The keys of the index by the places of their columns in the rows, which keep the key columns.
    let mut places = scanned.keys.clone();
    for key in &mut places {
        key.target = tables.place(r, key.target).unwrap_or(NONE);
    }
    places.retain(|key| key.target != NONE);
    let types = tables.stored_types(r);
    let keys = crate::sort::Keys::new(&places, &types, session)?;
    let mut error = None;
    ids.sort_by(|&x, &y| {
        let order = keys.compare(&rows[x], &rows[y]).unwrap_or_else(|e| {
            error.get_or_insert(e);
            Ordering::Equal
        });
        if scanned.backward { order.then(y.cmp(&x)) } else { order.then(x.cmp(&y)) }
    });
    if let Some(error) = error {
        return Err(error);
    }
    Ok(ids.into_iter().enumerate().map(|(rank, row)| (row, rank)).collect())
}

/// The order of the parts of `FROM` for the joins of [`tuples`], as `join_search_one_level` makes a join with no condition only when no other join has one: the written order, but when the next part has no `=` with the parts before it that a hash join can test, the first later part that has one comes first, so that the join does not make all the tuples of a cross join. `None` when the order is the written one. The parts keep the written order when a part reads other relations, as `LATERAL` lets it, or when the condition of a join in a part reads other parts.
fn join_order(
    items: &[(&FromItem, Vec<usize>)],
    conds: &[&Expr],
    tables: &Tables,
) -> Option<Vec<usize>> {
    if items.len() < 3
        || tables.needs.iter().any(|n| !n.is_empty())
        || items.iter().any(|(item, _)| !join_reads(item).is_empty())
    {
        return None;
    }
    let within =
        |used: &[usize], set: &[usize]| !used.is_empty() && used.iter().all(|r| set.contains(r));
    let links = |acc: &[usize], relations: &[usize]| {
        conds.iter().any(|expr| {
            let ExprKind::Func(f) = &expr.kind else { return false };
            if !HASH_EQUALS.contains(&f.oid) || f.args.len() != 2 {
                return false;
            }
            let (a, b) = (relations_of(&f.args[0]), relations_of(&f.args[1]));
            (within(&a, acc) && within(&b, relations)) || (within(&b, acc) && within(&a, relations))
        })
    };
    let mut rest: Vec<usize> = (1..items.len()).collect();
    let mut order = vec![0];
    let mut acc = items[0].1.clone();
    while !rest.is_empty() {
        let pick = rest.iter().position(|&i| links(&acc, &items[i].1)).unwrap_or(0);
        let i = rest.remove(pick);
        acc.extend(&items[i].1);
        order.push(i);
    }
    (!order.is_sorted()).then_some(order)
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
        // A part that reads only relations outside the join, which `context` holds, runs once for all the tuples of the left side. The condition of a join in the part can read the left side when the part comes from a `LATERAL` subquery that the planner pulled up.
        let reads = self.tables.depends_on(input.right_relations, input.left_relations)
            || join_reads(item).iter().any(|r| input.left_relations.contains(r));
        if !reads {
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
    fn lookup(
        &mut self,
        relation: usize,
        conds: &[&Expr],
        context: &[usize],
    ) -> Result<Option<Vec<usize>>> {
        let bound = bound(context, &[relation]);
        let found = conds.iter().find_map(|expr| {
            let ExprKind::Func(f) = &expr.kind else { return None };
            if !HASH_EQUALS.contains(&f.oid) || f.args.len() != 2 {
                return None;
            }
            // The other side can be a column of a relation of the outer side of the nested loop.
            let (a, b) = (&f.args[0], &f.args[1]);
            let pick = |column: &Expr, other: &Expr| {
                plain_var(column)
                    .is_some_and(|var| var.relation == relation && outer_expr(other, &bound))
            };
            if pick(a, b) {
                plain_var(a).map(|var| (var, b))
            } else if pick(b, a) {
                plain_var(b).map(|var| (var, a))
            } else {
                None
            }
        });
        let Some((var, outer)) = found else { return Ok(None) };
        // A null equals no value.
        let tuple = if bound.is_empty() { vec![NONE; self.width] } else { context.to_vec() };
        let Some(k) = key((self.test)(outer, &tuple)?) else {
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
                return keep_in(tuples, &conds, self.test, context);
            }
            FromItem::Relation(index) => {
                let rows = match self.lookup(*index, &conds, context)? {
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
                return keep_in(tuples, &conds, self.test, context);
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
        // A condition that reads one side and the relations of the outer side of a nested loop, which have one row for this run, reads that side only.
        let outside = bound(context, &[&left_relations[..], &right_relations].concat());
        let only = |expr: &Expr, side: &[usize]| {
            let relations = relations_of(expr);
            relations.iter().any(|r| side.contains(r))
                && relations.iter().all(|r| side.contains(r) || outside.contains(r))
        };
        // A condition on one side goes down to that side when the join keeps every row of that side. The other conditions of an inner join join the condition of the join, and those of an outer join test its result.
        let (mut to_left, mut to_right, mut after) = (Vec::new(), Vec::new(), Vec::new());
        let push_left = matches!(j.kind, JoinKind::Inner | JoinKind::Left);
        let push_right = matches!(j.kind, JoinKind::Inner | JoinKind::Right);
        for expr in conds {
            if push_left && only(expr, &left_relations) {
                to_left.push(expr);
            } else if push_right && only(expr, &right_relations) {
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
                if j.kind != JoinKind::Left && only(expr, &left_relations) {
                    to_left.push(expr);
                } else if j.kind != JoinKind::Right && only(expr, &right_relations) {
                    to_right.push(expr);
                } else {
                    kept.push(expr);
                }
            }
            on = kept;
        }
        let mut left = self.item_tuples(&j.left, to_left, context)?;
        // The condition of the join and the conditions after it can read the relations outside the join, whose rows the tuples then take from `context`.
        if !outside.is_empty() {
            for tuple in &mut left {
                *tuple = merged(context, tuple);
            }
        }
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

/// A join of two lists of tuples: a join through an index of the right side when the condition has an `=` that a hash can test, with a column or an expression of each side, or an `OR` of such `=`s, or else a nested loop. The rows of an outer join that match no row get nulls for the other side.
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
    let pair = |expr: &'a Expr| -> Option<(Side<'a>, Side<'a>)> {
        let ExprKind::Func(f) = &expr.kind else { return None };
        if !HASH_EQUALS.contains(&f.oid) || f.args.len() != 2 {
            return None;
        }
        let (a, b) = (&f.args[0], &f.args[1]);
        match (side(a, left_relations), side(b, right_relations)) {
            (Some(l), Some(r)) => Some((l, r)),
            _ => Some((side(b, left_relations)?, side(a, right_relations)?)),
        }
    };
    let pairs: Vec<(Side<'a>, Side<'a>)> = on.iter().filter_map(|expr| pair(expr)).collect();
    let hash = pairs
        .iter()
        .find(|(l, r)| matches!((l, r), (Side::Var(_), Side::Var(_))))
        .or_else(|| pairs.first());
    // With no such `=`, an `OR` of such `=`s gives the rows of the right side that match one of its arms, as a `BitmapOr` of index scans or an index scan with `= ANY` does.
    let arms: Vec<(Side<'a>, Side<'a>)> = if hash.is_none() {
        on.iter()
            .find_map(|expr| match &expr.kind {
                ExprKind::Bool(rupg_analyze::BoolOp::Or, args) => {
                    args.iter().map(pair).collect::<Option<Vec<_>>>()
                }
                _ => None,
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
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
    let mut arm_indexes: Vec<BTreeMap<Key, Vec<usize>>> = Vec::new();
    let mut arm_keys: Vec<Vec<Option<Key>>> = Vec::new();
    if !left.is_empty() {
        for (l, r) in &arms {
            let mut index: BTreeMap<Key, Vec<usize>> = BTreeMap::new();
            for (i, tuple) in right.iter().enumerate() {
                if let Some(k) = key(value(tuple, r)?) {
                    index.entry(k).or_default().push(i);
                }
            }
            let keys = left.iter().map(|tuple| Ok(key(value(tuple, l)?))).collect::<Result<_>>()?;
            arm_indexes.push(index);
            arm_keys.push(keys);
        }
    }
    let all: Vec<usize> = (0..right.len()).collect();
    let mut union = Vec::new();
    for (n, l) in left.iter().enumerate() {
        let candidates: &[usize] = match hash {
            Some(_) => match &left_keys[n] {
                Some(k) => index.get(k).map_or(&[], Vec::as_slice),
                None => &[],
            },
            None if !arm_indexes.is_empty() => {
                union.clear();
                for (index, keys) in arm_indexes.iter().zip(&arm_keys) {
                    if let Some(rows) = keys[n].as_ref().and_then(|k| index.get(k)) {
                        union.extend_from_slice(rows);
                    }
                }
                union.sort_unstable();
                union.dedup();
                &union
            }
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
