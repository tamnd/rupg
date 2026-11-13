//! The run of a correlated subquery once for all the rows of the query outside it, as the dependent join elimination of Neumann and Kemper (BTW 2015) unnests it (spec/15). The subquery must read the query outside it only through conditions of `WHERE` that are an `=` of an integer of the subquery and an integer column of the query just outside it. The run without these conditions gives the rows of the subquery with their keys, and each row of the query outside then reads the rows of its key from a hash table. A subquery with an aggregate and no `GROUP BY` runs with a `GROUP BY` of the keys, and a key with no group gets the row that the subquery gives for no input, so that `count(*)` gives 0 and not null.
//!
//! The vector scan of one table also tests an `EXISTS` of its `WHERE` with one key as a probe ([`Probe`]): of the set of the keys that have a row when the subquery has no other condition on the table, or of the bounds of [`Bound`]. The two scans keep the same state of the subquery, so it runs once also when the vector scan cannot run and the row engine scans the table.
//!
//! An `EXISTS` with one key whose other condition is one comparison of an integer column of the subquery with a column of the query outside it, such as `l2.l_suppkey <> l1.l_suppkey` in TPC-H Q21, keeps only the least and the greatest value of the column for each key ([`Bound`]). A row of the query outside it tests the comparison on these two values, and not on each row of its key.
//!
//! The subquery runs once at its first evaluation. The run for each row also reads all the rows of the tables of the subquery at the first row, into the cache of the tables, so the run once costs approximately the same for a query that reads only one row outside the subquery. The run of the subquery tests its conditions on rows that the old run did not test, so no expression of it can be volatile, and an error of the run makes the subquery run as before for each row, which then gives the same errors as before.

use std::rc::Rc;

use rupg_analyze::{BoolOp, Expr, ExprKind, Query, SortGroup, SubLinkKind, Target};
use rupg_common::Result;
use rupg_func::Session;
use rupg_types::{Array, Value, oid};

use crate::scan::{self, Key};
use crate::{Cache, Rows, agg, columnar, correlated, free_work, run_query, volatile, window};

/// The `=` functions of two integers: `int2eq`, `int4eq`, `int8eq`, `int24eq`, `int42eq`, `int48eq`, `int84eq`, `int28eq` and `int82eq`.
const EQUALS: [u32; 9] = [63, 65, 467, 158, 159, 852, 474, 1850, 1856];

/// The `<>`, `<`, `<=`, `>` and `>=` functions of two integers, in the order of the functions of [`EQUALS`].
const COMPARES: [(Cmp, [u32; 9]); 5] = [
    (Cmp::Ne, [145, 144, 468, 164, 165, 853, 475, 1851, 1857]),
    (Cmp::Lt, [64, 66, 469, 160, 161, 854, 476, 1852, 1858]),
    (Cmp::Le, [148, 149, 471, 166, 167, 856, 478, 1854, 1860]),
    (Cmp::Gt, [146, 147, 470, 162, 163, 855, 477, 1853, 1859]),
    (Cmp::Ge, [151, 150, 472, 168, 169, 857, 479, 1855, 1861]),
];

/// A comparison of two integers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cmp {
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Cmp {
    /// The comparison with the two sides in the other order.
    fn flip(self) -> Self {
        match self {
            Cmp::Ne => Cmp::Ne,
            Cmp::Lt => Cmp::Gt,
            Cmp::Le => Cmp::Ge,
            Cmp::Gt => Cmp::Lt,
            Cmp::Ge => Cmp::Le,
        }
    }

    /// True when the comparison of one value of a set with `value` is true, where `min` is the least value of the set and `max` the greatest. For these comparisons, that is true exactly when the comparison of `min` or of `max` with `value` is true.
    fn any(self, (min, max): (i64, i64), value: i64) -> bool {
        match self {
            Cmp::Ne => min != value || max != value,
            Cmp::Lt => min < value,
            Cmp::Le => min <= value,
            Cmp::Gt => max > value,
            Cmp::Ge => max >= value,
        }
    }
}

/// The result of the run of a lifted subquery: the rows of each key, or the set of the keys, or the bounds of each key, and the rows for a key that has no row.
type Ran = (Buckets, Option<KeySet>, Option<Bounds>, Rc<Rows>);

/// The least and the greatest value of the column of the test of [`Bound`] for each key, with no null.
enum Bounds {
    /// The bounds of each key in a hash table.
    Hash(rupg_kernels::hash::HashMap<i64, (i64, i64)>),
    /// The bounds of the key `base + i` in the slot `i`, for keys in a range that is not much larger than the number of rows, as the keys of a table that come from a sequence. A slot whose least value is greater than its greatest value has no row. A lookup reads one slot with no hash, and the rows of a table in the order of its key read the slots one after another.
    Dense { base: i64, slots: Vec<(i64, i64)> },
}

impl Bounds {
    /// The bounds of `key`, or `None` when no row has the key.
    fn get(&self, key: i64) -> Option<(i64, i64)> {
        match self {
            Bounds::Hash(map) => map.get(&key).copied(),
            Bounds::Dense { base, slots } => {
                let at = usize::try_from(key.checked_sub(*base)?).ok()?;
                slots.get(at).copied().filter(|(min, max)| min <= max)
            }
        }
    }

    /// The bounds of the keys `keys` with the values `values` of the same rows, where a row with a null key or value is not in a set. The bounds are [`Bounds::Dense`] when the range of the keys is at most twice the number of rows and 1024 more.
    fn of(keys: &[Option<i64>], values: &[Option<i64>]) -> Self {
        let mut low = i64::MAX;
        let mut high = i64::MIN;
        let mut count = 0usize;
        for (&key, &value) in keys.iter().zip(values) {
            if let (Some(key), Some(_)) = (key, value) {
                low = low.min(key);
                high = high.max(key);
                count += 1;
            }
        }
        let range = i128::from(high) - i128::from(low) + 1;
        let limit = i128::try_from(count).unwrap_or(i128::MAX).saturating_mul(2) + 1024;
        if count > 0
            && range <= limit
            && let Ok(range) = usize::try_from(range)
        {
            let mut slots = vec![(i64::MAX, i64::MIN); range];
            for (&key, &value) in keys.iter().zip(values) {
                if let (Some(key), Some(value)) = (key, value)
                    && let Ok(at) = usize::try_from(key - low)
                {
                    let slot = &mut slots[at];
                    slot.0 = slot.0.min(value);
                    slot.1 = slot.1.max(value);
                }
            }
            return Bounds::Dense { base: low, slots };
        }
        let mut bounds = Bounds::Hash(rupg_kernels::hash::HashMap::default());
        // The rows of a key often come one after another, as the rows of a table in the order of its key, so a run of the same key widens the bounds once.
        let mut run: Option<(i64, (i64, i64))> = None;
        for (&key, &value) in keys.iter().zip(values) {
            let (Some(key), Some(value)) = (key, value) else { continue };
            match &mut run {
                Some((k, (min, max))) if *k == key => {
                    *min = (*min).min(value);
                    *max = (*max).max(value);
                }
                _ => {
                    if let Some((k, found)) = run.replace((key, (value, value))) {
                        widen(&mut bounds, k, found);
                    }
                }
            }
        }
        if let Some((k, found)) = run {
            widen(&mut bounds, k, found);
        }
        bounds
    }
}

/// For an `EXISTS` with one integer key whose test is `column op outer`, where `column` is an integer column of the subquery and `outer` a column of the query outside it: `op`, `outer`, and the [`Bounds`]. A null `column` or `outer` makes the comparison not true, so the bounds do not keep the nulls, and a row whose `outer` is null gets false.
pub(crate) struct Bound {
    cmp: Cmp,
    outer: Expr,
    keys: Bounds,
}

/// The state of a correlated subquery in a run, as [`Cache`] keeps it.
pub(crate) enum State {
    /// The subquery runs for each row. The queries that ran keep their addresses, which the cache uses as keys.
    Never {
        _kept: Vec<Query>,
    },
    Lifted(Rc<Lifted>),
}

/// The rows of a correlated subquery for each key.
pub(crate) struct Lifted {
    /// The value of each key in the query outside the subquery: a column of that query.
    outer: Vec<Expr>,
    rows: Buckets,
    /// For an `EXISTS` with one integer key and no test, the keys that have a row, and `rows` is empty.
    set: Option<Rc<KeySet>>,
    /// For an `EXISTS` with one integer key whose test is one comparison of integers, the bounds of each key, and `rows` is empty.
    bound: Option<Bound>,
    /// The rows for a key that has no row.
    empty: Rc<Rows>,
    /// For `EXISTS`, the conditions that read the query outside the subquery and are not keys. They read the columns of the subquery as [`ExprKind::SubColumn`] of the rows of a key.
    test: Option<Expr>,
    /// The row of an `EXISTS` that a row satisfies.
    one: Rc<Rows>,
    /// The queries that ran, which keep their addresses, which the cache uses as keys.
    _queries: Vec<Query>,
}

/// An evaluation of an expression of the query outside a subquery, with a row of the subquery for [`ExprKind::SubColumn`].
pub(crate) type Evaluate<'a> = dyn FnMut(&Expr, Option<&[Value]>) -> Result<Value> + 'a;

/// The rows of a lifted subquery for each key.
type Buckets = rupg_kernels::hash::HashMap<Vec<Key>, Rc<Rows>>;

/// The keys of an `EXISTS` that have a row, for [`Probe`].
pub(crate) type KeySet = rupg_kernels::hash::HashSet<i64>;

/// The two queries of a lifted subquery and the value of each key outside it.
struct Split {
    /// The subquery without the conditions of the keys, with the keys as its last columns.
    all: Query,
    /// For a subquery with an aggregate, the subquery with no input row.
    empty: Option<Query>,
    outer: Vec<Expr>,
    test: Option<Expr>,
}

/// The rows of a subquery for the row of the query outside it that `eval` reads, or `None` when the subquery must run for the row. `eval` evaluates an expression of the query outside the subquery, with a row of the subquery for [`ExprKind::SubColumn`].
pub(crate) fn rows(
    query: &Query,
    kind: &SubLinkKind,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
    eval: &mut Evaluate<'_>,
) -> Result<Option<Rc<Rows>>> {
    let Some(lifted) = state(query, kind, params, session, cache) else { return Ok(None) };
    let mut keys = Vec::with_capacity(lifted.outer.len());
    for expr in &lifted.outer {
        let value = eval(expr, None)?;
        if value.is_null() {
            return Ok(Some(Rc::clone(&lifted.empty)));
        }
        match scan::key(value) {
            Some(k) => keys.push(k),
            None => return Ok(None),
        }
    }
    if let (Some(set), [Key::Int(key)]) = (&lifted.set, keys.as_slice()) {
        let found = if set.contains(key) { &lifted.one } else { &lifted.empty };
        return Ok(Some(Rc::clone(found)));
    }
    if let (Some(bound), [Key::Int(key)]) = (&lifted.bound, keys.as_slice()) {
        let found = match bound.keys.get(*key) {
            Some(bounds) => match scan::key(eval(&bound.outer, None)?) {
                Some(Key::Int(value)) => bound.cmp.any(bounds, value),
                _ => false,
            },
            None => false,
        };
        return Ok(Some(Rc::clone(if found { &lifted.one } else { &lifted.empty })));
    }
    let Some(rows) = lifted.rows.get(&keys) else { return Ok(Some(Rc::clone(&lifted.empty))) };
    let Some(test) = &lifted.test else { return Ok(Some(Rc::clone(rows))) };
    for row in rows.iter() {
        if eval(test, Some(row))?.as_bool() == Some(true) {
            return Ok(Some(Rc::clone(&lifted.one)));
        }
    }
    Ok(Some(Rc::clone(&lifted.empty)))
}

/// For the vector scan, a correlated `EXISTS` with one integer key, as [`probe`] gives it.
pub(crate) struct Probe {
    /// The column of the key in the query outside the subquery.
    pub(crate) outer: Expr,
    /// For the bounds of [`Bound`], the column of the query outside the subquery that the test compares.
    pub(crate) other: Option<Expr>,
    lifted: Rc<Lifted>,
}

impl Probe {
    /// The `EXISTS` for the rows `0..rows`, as [`rows`] gives it, where `key` gives the key of a row and `value` its column [`Probe::other`]: false for a null key, for a key with no row, and for a null `value`. Rows with the same key one after another, as the rows of a table in the order of its key come, look up the key once.
    pub(crate) fn tests(
        &self,
        rows: usize,
        key: impl Fn(usize) -> Option<i64>,
        value: impl Fn(usize) -> Option<i64>,
    ) -> Vec<Option<bool>> {
        let mut out = Vec::with_capacity(rows);
        if let Some(set) = &self.lifted.set {
            let mut last: Option<(i64, bool)> = None;
            for i in 0..rows {
                let found = key(i).is_some_and(|k| match last {
                    Some((l, found)) if l == k => found,
                    _ => {
                        let found = set.contains(&k);
                        last = Some((k, found));
                        found
                    }
                });
                out.push(Some(found));
            }
            return out;
        }
        let Some(bound) = &self.lifted.bound else {
            out.resize(rows, Some(false));
            return out;
        };
        let mut last: Option<(i64, Option<(i64, i64)>)> = None;
        for i in 0..rows {
            let found = key(i).is_some_and(|k| {
                let bounds = match last {
                    Some((l, bounds)) if l == k => bounds,
                    _ => {
                        let bounds = bound.keys.get(k);
                        last = Some((k, bounds));
                        bounds
                    }
                };
                bounds.zip(value(i)).is_some_and(|(bounds, v)| bound.cmp.any(bounds, v))
            });
            out.push(Some(found));
        }
        out
    }
}

/// For the vector scan, the probe of a correlated `EXISTS` with one integer key: with no other condition on the query outside it, the set of the keys that have a row, and with one comparison of integers, the bounds of [`Bound`]. `None` for another subquery, or when the run fails, and then the subquery runs for each row.
pub(crate) fn probe(
    query: &Query,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
) -> Option<Probe> {
    let lifted = state(query, &SubLinkKind::Exists, params, session, cache)?;
    let [outer] = lifted.outer.as_slice() else { return None };
    let other = match (&lifted.set, &lifted.bound, &lifted.test) {
        (Some(_), _, None) => None,
        (None, Some(bound), _) => Some(bound.outer.clone()),
        _ => return None,
    };
    Some(Probe { outer: outer.clone(), other, lifted: Rc::clone(&lifted) })
}

/// For the vector scan, a correlated subquery of one value with an aggregate and no `GROUP BY`, as [`scalar`] gives it.
pub(crate) struct Scalar {
    /// The columns of the keys in the query outside the subquery.
    pub(crate) outer: Vec<Expr>,
    /// The state of the subquery, which [`Scalar::run`] makes before the scan reads the link.
    lifted: Option<Rc<Lifted>>,
}

impl Scalar {
    /// True when the subquery ran.
    pub(crate) fn ready(&self) -> bool {
        self.lifted.is_some()
    }

    /// Runs the subquery `query` once with a `GROUP BY` of its keys. With `domain`, the values of each key in the rows of the query outside the subquery that read the link, the subquery reads only the rows of these keys, as the domain of the unnesting of Neumann and Kemper. The cache then keeps the state with no address of a subquery, so that no other run reads it. With no `domain`, the state is the one that [`rows`] reads. False when the subquery must run for each row.
    pub(crate) fn run(
        &mut self,
        query: &Query,
        params: &[Value],
        session: &dyn Session,
        cache: &Cache,
        domain: Option<&[KeySet]>,
    ) -> bool {
        let lifted = match domain {
            None => state(query, &SubLinkKind::Expr, params, session, cache),
            Some(domain) => {
                let state = lift(query, &SubLinkKind::Expr, params, session, cache, Some(domain));
                let lifted = match &state {
                    State::Lifted(lifted) => Some(Rc::clone(lifted)),
                    State::Never { .. } => None,
                };
                cache.lifted.borrow_mut().push((std::ptr::null(), state));
                lifted
            }
        };
        self.lifted = lifted.filter(|lifted| {
            lifted.empty.len() == 1
                && lifted.set.is_none()
                && lifted.bound.is_none()
                && lifted.test.is_none()
        });
        self.lifted.is_some()
    }

    /// The value of the subquery for a row whose keys are `keys`, as [`rows`] gives it: the value of the group of the keys, or the value of the subquery with no input row when a key is null or the keys have no group. Null before [`Scalar::run`].
    pub(crate) fn value(&self, keys: &[Option<i64>]) -> Value {
        let Some(lifted) = &self.lifted else { return Value::Null };
        let key: Option<Vec<Key>> = keys.iter().map(|k| k.map(Key::Int)).collect();
        let rows = key.and_then(|key| lifted.rows.get(&key)).unwrap_or(&lifted.empty);
        rows.first().and_then(|row| row.first()).cloned().unwrap_or(Value::Null)
    }
}

/// For the vector scan, a correlated subquery of one value with an aggregate and no `GROUP BY`, such as the `avg` of TPC-H Q17, which runs once with a `GROUP BY` of its integer keys. Each group and the subquery with no input row give one row. The subquery does not run here: [`Scalar::run`] runs it before the scan. `None` for another subquery, and then the subquery runs for each row.
pub(crate) fn scalar(query: &Query) -> Option<Scalar> {
    if !correlated(query) {
        return None;
    }
    let Split { empty, outer, test, .. } = split(query, &SubLinkKind::Expr)?;
    let one = empty.is_some()
        && test.is_none()
        && !outer.is_empty()
        && outer.iter().all(|e| matches!(e.ty, oid::INT2 | oid::INT4 | oid::INT8));
    one.then_some(Scalar { outer, lifted: None })
}

/// The lifted state of a subquery, which the first call makes and the cache keeps. `None` when the subquery runs for each row.
fn state(
    query: &Query,
    kind: &SubLinkKind,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
) -> Option<Rc<Lifted>> {
    let key: *const Query = query;
    let found = cache.lifted.borrow().iter().position(|(q, _)| *q == key);
    if let Some(at) = found {
        return match &cache.lifted.borrow()[at].1 {
            State::Never { .. } => None,
            State::Lifted(lifted) => Some(Rc::clone(lifted)),
        };
    }
    let state = if correlated(query) {
        lift(query, kind, params, session, cache, None)
    } else {
        State::Never { _kept: Vec::new() }
    };
    let lifted = match &state {
        State::Lifted(lifted) => Some(Rc::clone(lifted)),
        State::Never { .. } => None,
    };
    cache.lifted.borrow_mut().push((key, state));
    lifted
}

/// The state of a subquery at its first evaluation: its rows for each key, or [`State::Never`] when it cannot run once or when the run fails. With `domain`, the subquery reads only the rows of the keys of the domain, as [`narrow`] gives them.
fn lift(
    query: &Query,
    kind: &SubLinkKind,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
    domain: Option<&[KeySet]>,
) -> State {
    let Some(split) = split(query, kind) else { return State::Never { _kept: Vec::new() } };
    let Split { mut all, empty, outer, test } = split;
    if let Some(domain) = domain {
        narrow(&mut all, domain);
    }
    let mut queries = vec![all];
    queries.extend(empty);
    let width = outer.len();
    // With a test, `EXISTS` keeps the rows of a key, which the test reads.
    let collapse = *kind == SubLinkKind::Exists && test.is_none();
    let one = Rc::new(vec![Vec::new()]);
    // An `EXISTS` with one key and no test needs only the set of the keys that have a row.
    let single = collapse && width == 1;
    // An `EXISTS` with one key whose test is one comparison of integers needs only the bounds of the column for each key. The subquery then has the column of the test and the key as its two columns.
    let compare = test
        .as_ref()
        .filter(|_| width == 1 && queries.len() == 1 && queries[0].targets.len() == 2)
        .and_then(comparison);
    let run = || -> Result<Ran> {
        let empty_rows = match queries.get(1) {
            Some(q) => Rc::new(run_query(q, params, session, cache, &[], false)?),
            None => Rc::new(Vec::new()),
        };
        let columns = match single || compare.is_some() {
            true => columnar::column_keys(&queries[0], session, cache)?,
            false => None,
        };
        if compare.is_some() {
            let mut bounds = Bounds::Hash(rupg_kernels::hash::HashMap::default());
            match columns.as_deref() {
                Some([values, keys]) => bounds = Bounds::of(keys, values),
                Some(_) => return Err(rupg_common::Error::internal("a lifted row has no key")),
                None => {
                    for row in run_query(&queries[0], params, session, cache, &[], false)? {
                        let Ok([value, key]) = <[Value; 2]>::try_from(row) else {
                            return Err(rupg_common::Error::internal("a lifted row has no key"));
                        };
                        if let (Some(key), Some(value)) = (int(key)?, int(value)?) {
                            widen(&mut bounds, key, (value, value));
                        }
                    }
                }
            }
            return Ok((Buckets::default(), None, Some(bounds), empty_rows));
        }
        if single && let Some(keys) = columns.and_then(|mut columns| columns.pop()) {
            return Ok((
                Buckets::default(),
                Some(keys.into_iter().flatten().collect()),
                None,
                empty_rows,
            ));
        }
        let found = run_query(&queries[0], params, session, cache, &[], false)?;
        let mut grouped: rupg_kernels::hash::HashMap<Vec<Key>, Rows> = Default::default();
        let mut ints = KeySet::default();
        'row: for mut row in found {
            if single {
                let Some(value) = row.pop() else {
                    return Err(rupg_common::Error::internal("a lifted row has no key"));
                };
                if value.is_null() {
                    continue;
                }
                match scan::key(value) {
                    Some(Key::Int(key)) => {
                        ints.insert(key);
                    }
                    Some(key) => {
                        grouped.entry(vec![key]).or_default();
                    }
                    None => {
                        return Err(rupg_common::Error::internal("a lifted key is not an integer"));
                    }
                }
                continue;
            }
            let Some(columns) = row.len().checked_sub(width) else {
                return Err(rupg_common::Error::internal("a lifted row has no key"));
            };
            let mut keys = Vec::with_capacity(width);
            for value in row.drain(columns..) {
                if value.is_null() {
                    continue 'row;
                }
                match scan::key(value) {
                    Some(k) => keys.push(k),
                    None => {
                        return Err(rupg_common::Error::internal("a lifted key is not an integer"));
                    }
                }
            }
            let rows = grouped.entry(keys).or_default();
            if !collapse {
                rows.push(row);
            }
        }
        let set = if single && grouped.is_empty() {
            Some(ints)
        } else {
            grouped.extend(ints.into_iter().map(|key| (vec![Key::Int(key)], Vec::new())));
            None
        };
        let rows = grouped
            .into_iter()
            .map(|(k, rows)| (k, if collapse { Rc::clone(&one) } else { Rc::new(rows) }))
            .collect();
        Ok((rows, set, None, empty_rows))
    };
    let result = run();
    match result {
        Ok((rows, set, bounds, empty)) => {
            let set = set.map(Rc::new);
            let bound = compare.zip(bounds).map(|((cmp, outer), keys)| Bound { cmp, outer, keys });
            State::Lifted(Rc::new(Lifted {
                outer,
                rows,
                set,
                bound,
                empty,
                test,
                one,
                _queries: queries,
            }))
        }
        Err(_) => State::Never { _kept: queries },
    }
}

/// The comparison of the test of a [`Bound`] and the column of the query outside the subquery that it compares, or `None` for another test. One side must be the column of the subquery, `SubColumn(0)`, and the other side a column of the query outside it, which cannot give an error.
fn comparison(test: &Expr) -> Option<(Cmp, Expr)> {
    let ExprKind::Func(f) = &test.kind else { return None };
    let [a, b] = f.args.as_slice() else { return None };
    let cmp = COMPARES.iter().find(|(_, oids)| oids.contains(&f.oid))?.0;
    match (&a.kind, &b.kind) {
        (ExprKind::SubColumn(0), ExprKind::Var(_)) => Some((cmp, b.clone())),
        (ExprKind::Var(_), ExprKind::SubColumn(0)) => Some((cmp.flip(), a.clone())),
        _ => None,
    }
}

/// Adds the least and the greatest of some values of the column of the test of a [`Bound`] to the bounds of their key.
fn widen(bounds: &mut Bounds, key: i64, (low, high): (i64, i64)) {
    let Bounds::Hash(map) = bounds else { return };
    map.entry(key)
        .and_modify(|(min, max)| {
            *min = (*min).min(low);
            *max = (*max).max(high);
        })
        .or_insert((low, high));
}

/// The integer of a value of a lifted row, or `None` for a null.
fn int(value: Value) -> Result<Option<i64>> {
    if value.is_null() {
        return Ok(None);
    }
    match scan::key(value) {
        Some(Key::Int(v)) => Ok(Some(v)),
        _ => Err(rupg_common::Error::internal("a lifted value is not an integer")),
    }
}

/// True when an expression reads a column of a query outside the query at `depth`.
fn reads_outer(expr: &Expr, depth: usize) -> bool {
    expr.find(depth, &mut |e, depth| match e.kind {
        ExprKind::Var(var) if var.levels_up > depth => Some(()),
        _ => None,
    })
    .is_some()
}

/// The column of the query just outside a subquery that an expression of the subquery is, with no other type of the same form.
fn outer_column(expr: &Expr) -> Option<Expr> {
    match &expr.kind {
        ExprKind::Var(var) if var.levels_up == 1 => {
            let mut var = *var;
            var.levels_up = 0;
            Some(Expr { kind: ExprKind::Var(var), ..expr.clone() })
        }
        ExprKind::Relabel(arg, _) => outer_column(arg),
        _ => None,
    }
}

/// Adds to the subquery `all` of a lift the condition that its key with the fewest values in `domain` is one of these values. `domain` has the values of each key in the rows of the query outside the subquery. So the subquery makes no group for other keys, and a row of these keys gets the same value as before. The condition `x = ANY (array)` is a test of the zones of the column.
fn narrow(all: &mut Query, domain: &[KeySet]) {
    let Some((n, keys)) = domain.iter().enumerate().min_by_key(|(_, keys)| keys.len()) else {
        return;
    };
    let name = format!("?key{n}?");
    let Some(target) = all.targets.iter().find(|t| t.name == name) else { return };
    let inner = target.expr.clone();
    let Some((_, _, equal)) = int_operators(inner.ty) else { return };
    let mut ints: Vec<i64> = keys.iter().copied().collect();
    ints.sort_unstable();
    let (values, array): (Vec<Option<Value>>, u32) = match inner.ty {
        oid::INT2 => (
            ints.into_iter()
                .filter_map(|k| i16::try_from(k).ok())
                .map(|k| Some(Value::Int2(k)))
                .collect(),
            oid::INT2_ARRAY,
        ),
        oid::INT4 => (
            ints.into_iter()
                .filter_map(|k| i32::try_from(k).ok())
                .map(|k| Some(Value::Int4(k)))
                .collect(),
            oid::INT4_ARRAY,
        ),
        _ => (ints.into_iter().map(|k| Some(Value::Int8(k))).collect(), oid::INT8_ARRAY),
    };
    let constant = Expr {
        kind: ExprKind::Const(Value::Array(Box::new(Array::one(values)))),
        ty: array,
        typmod: -1,
        location: None,
        collation: 0,
    };
    let cond = Expr {
        kind: ExprKind::ScalarArrayOp {
            func: equal,
            any: true,
            args: vec![inner, constant],
            collation: 0,
        },
        ty: oid::BOOL,
        typmod: -1,
        location: None,
        collation: 0,
    };
    all.filter = and(all.filter.take().into_iter().chain([cond]).collect());
}

/// The `<` operator, the `<` function and the `=` function of an integer type.
fn int_operators(ty: u32) -> Option<(u32, u32, u32)> {
    match ty {
        oid::INT2 => Some((95, 64, 63)),
        oid::INT4 => Some((97, 66, 65)),
        oid::INT8 => Some((412, 469, 467)),
        _ => None,
    }
}

/// The queries of a subquery that can run once, or `None` when it must run for each row.
fn split(query: &Query, kind: &SubLinkKind) -> Option<Split> {
    let exists = *kind == SubLinkKind::Exists;
    let aggs = !agg::calls(query).is_empty();
    // `EXISTS` needs only a row, so a `LIMIT` that is a constant more than 0 does not change it.
    let limit_ok = match &query.limit {
        None => true,
        Some(limit) => {
            exists && !aggs && matches!(limit.kind, ExprKind::Const(Value::Int8(n)) if n > 0)
        }
    };
    if !limit_ok
        || query.set_op.is_some()
        || !window::calls(query).is_empty()
        || query.has_target_srfs()
        || !query.ctes.is_empty()
        || query.recursive
        || !query.row_marks.is_empty()
        || query.write.is_some()
        || query.offset.is_some()
        || !query.distinct.is_empty()
        || !query.group.is_empty()
        || !query.grouping_sets.is_empty()
        || query.having.is_some()
        || !query.barrier.is_empty()
        || !query.security.is_empty()
        || !agg::outer_calls(query).is_empty()
        || !free_work(query).is_empty()
        || (aggs && !matches!(kind, SubLinkKind::Expr | SubLinkKind::Exists))
    {
        return None;
    }
    let filter = query.filter.as_ref()?;
    let mut conds = Vec::new();
    scan::conjuncts(filter, &mut conds);
    let (mut kept, mut inner, mut outer, mut other) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for cond in conds {
        if !reads_outer(cond, 0) {
            kept.push(cond.clone());
        } else if let Some((side, column)) = key(cond) {
            inner.push(side);
            outer.push(column);
        } else {
            other.push(cond.clone());
        }
    }
    // Only `EXISTS` with no aggregate can test the other conditions on the rows of a key.
    if inner.is_empty() || (!other.is_empty() && !(exists && !aggs)) {
        return None;
    }
    let mut all = query.clone();
    all.filter = and(kept);
    all.minmax = None;
    let mut test = None;
    if exists && !aggs {
        all.targets.clear();
        all.sort.clear();
        all.limit = None;
        if let Some(cond) = and(other) {
            let (cond, columns) = outer_test(cond)?;
            all.targets.extend(columns.into_iter().enumerate().map(|(n, expr)| Target {
                name: format!("?test{n}?"),
                expr,
                origin: None,
                junk: false,
            }));
            test = Some(cond);
        }
    }
    if aggs {
        all.sort.clear();
    }
    // The keys come after the columns of the subquery and before its junk columns, which the run does not give, so the sort moves to the new places of the junk columns.
    let first = all.targets.iter().position(|t| t.junk).unwrap_or(all.targets.len());
    let count = inner.len();
    for (n, expr) in inner.into_iter().enumerate() {
        let target = Target { name: format!("?key{n}?"), expr, origin: None, junk: false };
        all.targets.insert(first + n, target);
    }
    for item in &mut all.sort {
        if item.target >= first {
            item.target += count;
        }
    }
    let mut empty = None;
    if aggs {
        all.grouped = true;
        for (n, target) in all.targets.iter().enumerate().skip(first).take(count) {
            let (operator, sort, equal) = int_operators(target.expr.ty)?;
            all.group.push(SortGroup {
                target: n,
                operator,
                sort,
                equal,
                nulls_first: false,
                hashable: true,
                collation: 0,
            });
        }
        let mut none = query.clone();
        none.filter = Some(Expr {
            collation: 0,
            kind: ExprKind::Const(Value::Bool(false)),
            ty: oid::BOOL,
            typmod: -1,
            location: None,
        });
        none.minmax = None;
        none.sort.clear();
        if correlated(&none) {
            return None;
        }
        empty = Some(none);
    }
    let still = correlated(&all);
    let unstable = all.all_exprs(0).iter().any(|(e, _)| volatile(e));
    if still || unstable {
        return None;
    }
    Some(Split { all, empty, outer, test })
}

/// The column of the subquery and the column of the query outside it that a condition of the subquery tests for `=`, when the condition can be a key.
fn key(cond: &Expr) -> Option<(Expr, Expr)> {
    let ExprKind::Func(f) = &cond.kind else { return None };
    let [a, b] = f.args.as_slice() else { return None };
    if !EQUALS.contains(&f.oid) {
        return None;
    }
    let (side, column) = match (outer_column(a), outer_column(b)) {
        (None, Some(column)) => (a, column),
        (Some(column), None) => (b, column),
        _ => return None,
    };
    let plain = !reads_outer(side, 0)
        && side.find(0, &mut |e, _| matches!(e.kind, ExprKind::SubLink(_)).then_some(())).is_none();
    if !plain || int_operators(side.ty).is_none() || int_operators(column.ty).is_none() {
        return None;
    }
    Some((side.clone(), column))
}

/// The `AND` of conditions, or `None` for no condition.
fn and(mut conds: Vec<Expr>) -> Option<Expr> {
    match conds.len() {
        0 => None,
        1 => conds.pop(),
        _ => Some(Expr {
            collation: 0,
            kind: ExprKind::Bool(BoolOp::And, conds),
            ty: oid::BOOL,
            typmod: -1,
            location: None,
        }),
    }
}

/// A condition of a subquery as a test in the query just outside it, and the columns of the subquery that the test reads as [`ExprKind::SubColumn`]. `None` when the condition reads more than the columns of the two queries and constants, or is volatile.
fn outer_test(mut cond: Expr) -> Option<(Expr, Vec<Expr>)> {
    let mut columns: Vec<Expr> = Vec::new();
    let mut ok = !volatile(&cond);
    cond.each_mut(0, &mut |e, _| {
        if matches!(
            e.kind,
            ExprKind::SubLink(_)
                | ExprKind::Agg(_)
                | ExprKind::PlaceHolder(_)
                | ExprKind::SubColumn(_)
        ) {
            ok = false;
            return true;
        }
        let ExprKind::Var(var) = e.kind else { return false };
        match var.levels_up {
            0 => {
                let at = columns.iter().position(|c| c == e).unwrap_or_else(|| {
                    columns.push(e.clone());
                    columns.len() - 1
                });
                e.kind = ExprKind::SubColumn(at);
            }
            1 => e.kind = ExprKind::Var(rupg_analyze::Var { levels_up: 0, ..var }),
            _ => ok = false,
        }
        true
    });
    ok.then_some((cond, columns))
}
