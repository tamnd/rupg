//! `GROUP BY`, `HAVING` and the aggregate functions, as `nodeAgg.c` computes them.
//!
//! The rows of each group come in the order of the scan, as with the hash table of `HashAggregate`. When every key of `GROUP BY` has an order, the groups come in the order of the keys. Else they come in the order of their first rows. A query with no `GROUP BY` has one group, also when it has no rows.

use rupg_analyze::{Aggref, Expr, ExprKind, Query, SortGroup};
use rupg_common::{Error, Result};
use rupg_func::{Call, Kernel};
use rupg_pgcatalog::builtin;
use rupg_types::{Array, Numeric, NumericSign, Value, oid};

use crate::{Eval, kernel, not_yet, signature, sort, strict};

/// The way the engine computes an aggregate.
pub(crate) enum Method {
    /// The transition function and the final function are kernels, and the state is a value of SQL.
    Kernel {
        trans: Kernel,
        trans_strict: bool,
        /// The type of the state. For a polymorphic state it is the type of the result.
        state: u32,
        /// `agginitval`, the text of the first state, or `None` for null.
        init: Option<&'static str>,
        /// The final function and true when it is strict.
        last: Option<(Kernel, bool)>,
    },
    /// `int8_avg_accum`: the sum of `bigint` values as a 128-bit integer, for `sum` or for `avg`.
    Int128 { avg: bool },
    /// `numeric_avg_accum`: the sum of `numeric` values, for `sum` or for `avg`.
    Numeric { avg: bool },
    /// `string_agg` of `text` or of `bytea`.
    Concat { bytea: bool },
    /// `array_agg` of values that are not arrays.
    ArrayAgg,
}

/// True for a pseudo-type that takes the type of the arguments.
fn polymorphic(ty: u32) -> bool {
    matches!(
        ty,
        oid::ANYELEMENT
            | oid::ANYARRAY
            | oid::ANYNONARRAY
            | oid::ANYENUM
            | oid::ANYRANGE
            | oid::ANYMULTIRANGE
            | oid::ANYCOMPATIBLE
            | oid::ANYCOMPATIBLEARRAY
            | oid::ANYCOMPATIBLENONARRAY
            | oid::ANYCOMPATIBLERANGE
            | oid::ANYCOMPATIBLEMULTIRANGE
    )
}

/// The method of an aggregate call whose result has the type `ty`, or the error `0A000` for an aggregate that the engine does not have yet.
pub(crate) fn method(agg: &Aggref, ty: u32, location: Option<usize>) -> Result<Method> {
    let unsupported = || {
        let error = not_yet(format!("aggregate function {}", signature(agg.oid)));
        match location {
            Some(at) => error.at(at),
            None => error,
        }
    };
    let row = builtin::aggregate(agg.oid).ok_or_else(unsupported)?;
    let src = |func: u32| builtin::proc_by_oid(func).map_or("", |p| p.src);
    Ok(match (src(row.transfn), src(row.finalfn)) {
        ("int8_avg_accum", "numeric_poly_sum") => Method::Int128 { avg: false },
        ("int8_avg_accum", "numeric_poly_avg") => Method::Int128 { avg: true },
        ("numeric_avg_accum", "numeric_sum") => Method::Numeric { avg: false },
        ("numeric_avg_accum", "numeric_avg") => Method::Numeric { avg: true },
        ("string_agg_transfn", "string_agg_finalfn") => Method::Concat { bytea: false },
        ("bytea_string_agg_transfn", "bytea_string_agg_finalfn") => Method::Concat { bytea: true },
        ("array_agg_transfn", "array_agg_finalfn") => Method::ArrayAgg,
        _ => {
            let state =
                if polymorphic(row.transtype) && row.finalfn == 0 { ty } else { row.transtype };
            if state == oid::INTERNAL || polymorphic(state) {
                return Err(unsupported());
            }
            if row.initval.is_some() && !rupg_func::input_supported(state) {
                return Err(unsupported());
            }
            let trans = rupg_func::kernel(row.transfn).ok_or_else(unsupported)?;
            let last = match row.finalfn {
                0 => None,
                f => Some((rupg_func::kernel(f).ok_or_else(unsupported)?, strict(f))),
            };
            Method::Kernel {
                trans,
                trans_strict: strict(row.transfn),
                state,
                init: row.initval,
                last,
            }
        }
    })
}

/// The state of an aggregate in a group.
enum State {
    /// The state of [`Method::Kernel`]. `empty` is `noTransValue`: the state is null and the first value that is not null becomes the state.
    Value {
        value: Value,
        empty: bool,
    },
    Int128 {
        count: i64,
        sum: i128,
    },
    Numeric {
        count: i64,
        nan: i64,
        pinf: i64,
        ninf: i64,
        sum: Option<Numeric>,
    },
    /// The text so far, and the length of the first delimiter, which the result does not have.
    Concat {
        data: Option<Vec<u8>>,
        cut: usize,
    },
    ArrayAgg {
        values: Option<Vec<Option<Value>>>,
    },
}

/// An aggregate call of the query with its method.
struct Agg<'q> {
    agg: &'q Aggref,
    ty: u32,
    method: Method,
    /// The types of the arguments that the transition function gets.
    types: Vec<u32>,
}

impl Agg<'_> {
    fn start(&self, eval: &Eval<'_>) -> Result<State> {
        Ok(match &self.method {
            Method::Kernel { state, init, .. } => match init {
                Some(text) => State::Value {
                    value: rupg_func::input(*state, text, -1, eval.session)?,
                    empty: false,
                },
                None => State::Value { value: Value::Null, empty: true },
            },
            Method::Int128 { .. } => State::Int128 { count: 0, sum: 0 },
            Method::Numeric { .. } => {
                State::Numeric { count: 0, nan: 0, pinf: 0, ninf: 0, sum: None }
            }
            Method::Concat { .. } => State::Concat { data: None, cut: 0 },
            Method::ArrayAgg => State::ArrayAgg { values: None },
        })
    }

    /// `advance_transition_function`: adds the values of the arguments of one row to the state.
    fn advance(&self, state: &mut State, args: &[Value], eval: &Eval<'_>) -> Result<()> {
        let args = &args[..self.agg.nargs];
        match (&self.method, state) {
            (
                Method::Kernel { trans, trans_strict, state: ty, .. },
                State::Value { value, empty },
            ) => {
                if *trans_strict {
                    if args.iter().any(Value::is_null) {
                        return Ok(());
                    }
                    if *empty && let Some(first) = args.first() {
                        *value = first.clone();
                        *empty = false;
                        return Ok(());
                    }
                    if value.is_null() {
                        return Ok(());
                    }
                }
                let mut types = Vec::with_capacity(args.len() + 1);
                types.push(*ty);
                types.extend(&self.types);
                let mut values = Vec::with_capacity(args.len() + 1);
                values.push(std::mem::replace(value, Value::Null));
                values.extend_from_slice(args);
                let call = Call { session: eval.session, args: &types, ret: *ty, variadic: false };
                *value = trans(&call, &values)?;
                *empty = false;
            }
            (Method::Int128 { .. }, State::Int128 { count, sum }) => {
                if let Some(v) = args.first().and_then(Value::as_i64) {
                    *count += 1;
                    *sum += i128::from(v);
                }
            }
            (Method::Numeric { .. }, State::Numeric { count, nan, pinf, ninf, sum }) => {
                match args.first() {
                    Some(Value::Numeric(v)) => match v.sign() {
                        NumericSign::NaN => *nan += 1,
                        NumericSign::Infinity => *pinf += 1,
                        NumericSign::NegativeInfinity => *ninf += 1,
                        _ => {
                            *count += 1;
                            *sum = Some(match sum.take() {
                                Some(s) => s.add(v).map_err(rupg_func::type_error)?,
                                None => v.clone(),
                            });
                        }
                    },
                    Some(Value::Null) => {}
                    _ => {
                        return Err(Error::internal(
                            "sum of numeric got a value of the wrong kind",
                        ));
                    }
                }
            }
            (Method::Concat { .. }, State::Concat { data, cut }) => {
                let bytes = |v: &Value| match v {
                    Value::Text(s) => Some(s.as_bytes().to_vec()),
                    Value::Bytea(b) => Some(b.clone()),
                    _ => None,
                };
                let [value, delimiter] = args else {
                    return Err(Error::internal("string_agg needs two arguments"));
                };
                if value.is_null() {
                    return Ok(());
                }
                let first = data.is_none();
                let out = data.get_or_insert_with(Vec::new);
                if let Some(delimiter) = bytes(delimiter) {
                    if first {
                        *cut = delimiter.len();
                    }
                    out.extend(delimiter);
                }
                out.extend(
                    bytes(value).ok_or_else(|| {
                        Error::internal("string_agg got a value of the wrong kind")
                    })?,
                );
            }
            (Method::ArrayAgg, State::ArrayAgg { values }) => {
                let value = args.first().cloned().unwrap_or(Value::Null);
                values.get_or_insert_with(Vec::new).push((!value.is_null()).then_some(value));
            }
            _ => return Err(Error::internal("an aggregate state of the wrong kind")),
        }
        Ok(())
    }

    /// `finalize_aggregate`: the result of the aggregate from its state.
    fn finish(&self, state: State, eval: &Eval<'_>) -> Result<Value> {
        let numeric = |v: i128| Numeric::from_integer(v);
        let divide = |sum: Numeric, count: i64| {
            sum.div(&numeric(i128::from(count))).map(Value::Numeric).map_err(rupg_func::type_error)
        };
        match (&self.method, state) {
            (Method::Kernel { last, state: ty, .. }, State::Value { value, .. }) => match last {
                None => Ok(value),
                Some((_, true)) if value.is_null() => Ok(Value::Null),
                Some((f, _)) => {
                    let types = [*ty];
                    let call =
                        Call { session: eval.session, args: &types, ret: self.ty, variadic: false };
                    f(&call, &[value])
                }
            },
            (Method::Int128 { avg }, State::Int128 { count, sum }) => match (count, avg) {
                (0, _) => Ok(Value::Null),
                (_, false) => Ok(Value::Numeric(numeric(sum))),
                (_, true) => divide(numeric(sum), count),
            },
            (Method::Numeric { avg }, State::Numeric { count, nan, pinf, ninf, sum }) => {
                if count + nan + pinf + ninf == 0 {
                    Ok(Value::Null)
                } else if nan > 0 || (pinf > 0 && ninf > 0) {
                    Ok(Value::Numeric(Numeric::NAN))
                } else if pinf > 0 {
                    Ok(Value::Numeric(Numeric::INFINITY))
                } else if ninf > 0 {
                    Ok(Value::Numeric(Numeric::NEGATIVE_INFINITY))
                } else {
                    let sum = sum.unwrap_or_else(|| numeric(0));
                    if *avg { divide(sum, count) } else { Ok(Value::Numeric(sum)) }
                }
            }
            (Method::Concat { bytea }, State::Concat { data, cut }) => match data {
                None => Ok(Value::Null),
                Some(mut data) => {
                    data.drain(..cut.min(data.len()));
                    if *bytea {
                        Ok(Value::Bytea(data))
                    } else {
                        String::from_utf8(data)
                            .map(Value::Text)
                            .map_err(|_| Error::internal("string_agg made text that is not UTF-8"))
                    }
                }
            },
            (Method::ArrayAgg, State::ArrayAgg { values }) => Ok(match values {
                None => Value::Null,
                Some(values) => Value::Array(Box::new(Array::one(values))),
            }),
            _ => Err(Error::internal("an aggregate state of the wrong kind")),
        }
    }
}

/// The aggregate calls of an expression. An aggregate call has no other aggregate call in it.
fn collect<'q>(expr: &'q Expr, out: &mut Vec<(&'q Aggref, &'q Expr)>) {
    if let ExprKind::Agg(agg) = &expr.kind {
        out.push((agg, expr));
        return;
    }
    for child in expr.children() {
        collect(child, out);
    }
}

/// The aggregate calls of the select list and of `HAVING`.
pub(crate) fn calls(query: &Query) -> Vec<(&Aggref, &Expr)> {
    let mut out = Vec::new();
    for target in &query.targets {
        collect(&target.expr, &mut out);
    }
    if let Some(having) = &query.having {
        collect(having, &mut out);
    }
    out
}

/// Checks that the engine has every aggregate and every function of the groups.
pub(crate) fn check(query: &Query) -> Result<()> {
    for (agg, expr) in calls(query) {
        method(agg, expr.ty, expr.location)?;
        for g in agg.order.iter().chain(&agg.distinct) {
            kernel(g.equal, None)?;
            if g.sort != 0 {
                kernel(g.sort, None)?;
            }
        }
    }
    for g in &query.group {
        kernel(g.equal, None)?;
        if g.sort != 0 {
            kernel(g.sort, None)?;
        }
    }
    Ok(())
}

/// The groups of the rows, as the indexes of the rows of each group.
fn groups<'a>(
    eval: &mut Eval<'a>,
    query: &Query,
    tuples: &'a [Vec<usize>],
) -> Result<Vec<Vec<usize>>> {
    if query.group.is_empty() {
        return Ok(vec![(0..tuples.len()).collect()]);
    }
    let exprs: Vec<&Expr> = query.group.iter().map(|g| &query.targets[g.target].expr).collect();
    let mut keys = Vec::with_capacity(tuples.len());
    for tuple in tuples {
        eval.tuple = tuple;
        keys.push(exprs.iter().map(|e| eval.eval(e)).collect::<Result<Vec<_>>>()?);
    }
    let list: Vec<SortGroup> =
        query.group.iter().enumerate().map(|(i, g)| SortGroup { target: i, ..g.clone() }).collect();
    let types: Vec<u32> = exprs.iter().map(|e| e.ty).collect();
    let keys_of = sort::Keys::new(&list, &types, eval.session)?;
    let mut groups: Vec<Vec<usize>> = Vec::new();
    if list.iter().all(|g| g.sort != 0) {
        let mut order: Vec<usize> = (0..tuples.len()).collect();
        sort::qsort(&mut order, &mut |a, b| {
            Ok(keys_of.compare(&keys[*a], &keys[*b])?.then(a.cmp(b)))
        })?;
        for i in order {
            match groups.last_mut() {
                Some(group) if keys_of.equal(&keys[group[0]], &keys[i])? => group.push(i),
                _ => groups.push(vec![i]),
            }
        }
    } else {
        for i in 0..tuples.len() {
            let mut found = None;
            for (n, group) in groups.iter().enumerate() {
                if keys_of.equal(&keys[group[0]], &keys[i])? {
                    found = Some(n);
                    break;
                }
            }
            match found {
                Some(n) => groups[n].push(i),
                None => groups.push(vec![i]),
            }
        }
    }
    Ok(groups)
}

/// The rows of a query with `GROUP BY`, `HAVING` or an aggregate call: one row for each group that `HAVING` keeps, with all the items of the select list.
pub(crate) fn rows<'a>(
    eval: &mut Eval<'a>,
    query: &'a Query,
    tuples: &'a [Vec<usize>],
    none: &'a [usize],
) -> Result<Vec<Vec<Value>>> {
    let aggs: Vec<Agg<'_>> = calls(query)
        .into_iter()
        .map(|(agg, expr)| {
            Ok(Agg {
                agg,
                ty: expr.ty,
                method: method(agg, expr.ty, expr.location)?,
                types: agg.args[..agg.nargs].iter().map(|a| a.ty).collect(),
            })
        })
        .collect::<Result<_>>()?;
    let levels = super::set_levels(&query.targets);
    let mut rows = Vec::new();
    for group in groups(eval, query, tuples)? {
        let mut states = aggs.iter().map(|a| a.start(eval)).collect::<Result<Vec<_>>>()?;
        // The rows of an aggregate with `DISTINCT` or `ORDER BY`, which it reads after the sort.
        let mut sorted: Vec<Vec<Vec<Value>>> = vec![Vec::new(); aggs.len()];
        for &i in &group {
            eval.tuple = &tuples[i];
            for ((agg, state), sorted) in aggs.iter().zip(&mut states).zip(&mut sorted) {
                if let Some(filter) = &agg.agg.filter
                    && eval.eval(filter)? != Value::Bool(true)
                {
                    continue;
                }
                let args = agg.agg.args.iter().map(|a| eval.eval(a)).collect::<Result<Vec<_>>>()?;
                if agg.agg.order.is_empty() && agg.agg.distinct.is_empty() {
                    agg.advance(state, &args, eval)?;
                } else {
                    sorted.push(args);
                }
            }
        }
        let mut values = Vec::with_capacity(aggs.len());
        for ((agg, mut state), mut inputs) in aggs.iter().zip(states).zip(sorted) {
            if !agg.agg.order.is_empty() || !agg.agg.distinct.is_empty() {
                // `process_ordered_aggregate_multi`: a `DISTINCT` sorts on its own items, which start with the items of `ORDER BY`.
                let list =
                    if agg.agg.distinct.is_empty() { &agg.agg.order } else { &agg.agg.distinct };
                let types: Vec<u32> = agg.agg.args.iter().map(|a| a.ty).collect();
                let keys = sort::Keys::new(list, &types, eval.session)?;
                sort::qsort(&mut inputs, &mut |a, b| keys.compare(a, b))?;
                let mut last: Option<&Vec<Value>> = None;
                for args in &inputs {
                    if !agg.agg.distinct.is_empty() {
                        if let Some(last) = last
                            && keys.equal(last, args)?
                        {
                            continue;
                        }
                        last = Some(args);
                    }
                    agg.advance(&mut state, args, eval)?;
                }
            }
            values.push((std::ptr::from_ref(agg.agg), agg.finish(state, eval)?));
        }
        eval.aggs = values;
        eval.tuple = group.first().map_or(none, |&i| &tuples[i]);
        if let Some(having) = &query.having
            && eval.eval(having)? != Value::Bool(true)
        {
            continue;
        }
        eval.project(&query.targets, &levels, &mut rows)?;
    }
    eval.aggs.clear();
    Ok(rows)
}
