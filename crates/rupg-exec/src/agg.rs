//! `GROUP BY`, `HAVING` and the aggregate functions, as `nodeAgg.c` computes them.
//!
//! The rows of each group come in the order of the scan, as with the hash table of `HashAggregate`. When the plan has a `HashAggregate` for `GROUP BY`, the groups come in the order of its hash table. Else, when every key of `GROUP BY` has an order, the groups come in the order of the keys, and else in the order of their first rows. A query with no `GROUP BY` has one group, also when it has no rows.

use std::collections::HashSet;

use rupg_analyze::{
    Aggref, Expr, ExprKind, Frame, FrameBound, Query, SortGroup, WindowFunc, expand_grouping_sets,
};
use rupg_common::{Error, Result, SqlState};
use rupg_func::{Call, Kernel};
use rupg_pgcatalog::builtin;
use rupg_types::{Array, ArrayDim, MAXDIM, Numeric, NumericSign, Range, Value, oid};

use crate::{Eval, Group, hashed, kernel, not_yet, signature, sort, strict, window};

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
    /// The transition function or the final function is not a kernel, as a function in SQL of `CREATE AGGREGATE` is not, and the run calls the functions by their OIDs. The state is a value of SQL.
    Function {
        trans: u32,
        trans_strict: bool,
        state: u32,
        init: Option<&'static str>,
        /// The final function, true when it is strict, and `aggfinalextra`: true when it gets a null for each argument of the aggregate after the state.
        last: Option<(u32, bool, bool)>,
    },
    /// `int8_avg_accum`: the sum of `bigint` values as a 128-bit integer, for `sum` or for `avg`.
    Int128 { avg: bool },
    /// `numeric_avg_accum`: the sum of `numeric` values, for `sum` or for `avg`.
    Numeric { avg: bool },
    /// `string_agg` of `text` or of `bytea`.
    Concat { bytea: bool },
    /// `array_agg` of values that are not arrays.
    ArrayAgg,
    /// `array_agg` of arrays, which makes an array with one more dimension.
    ArrayAggArray,
    /// `range_agg` of ranges or of multiranges, which makes a multirange.
    RangeAgg,
    /// `int2_accum`, `int4_accum`, `int8_accum` and `numeric_accum`: the count, the sum and the sum of squares, for the variance of a sample or of the population, or for its square root when `root` is set.
    Moments { sample: bool, root: bool },
    /// `json_agg`, `json_object_agg` and the forms of `jsonb`. `strict` skips the null values, and `unique` makes a key that comes twice an error.
    Json { object: bool, jsonb: bool, strict: bool, unique: bool },
    /// `interval_avg_accum`: the sum of `interval` values, for `sum` or for `avg`.
    Interval { avg: bool },
    /// An ordered-set or a hypothetical-set aggregate, which reads the sorted rows of the group and its direct arguments.
    OrderedSet(Ordered),
}

/// The final function of an ordered-set or a hypothetical-set aggregate, from `orderedsetaggs.c`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ordered {
    /// `percentile_disc`, of an array of percentiles when `multi` is set.
    Disc { multi: bool },
    /// `percentile_cont` of `double precision`, or of `interval` when `interval` is set.
    Cont { multi: bool, interval: bool },
    /// `mode`.
    Mode,
    /// `rank` of a hypothetical row.
    Rank,
    /// `percent_rank` of a hypothetical row.
    PercentRank,
    /// `cume_dist` of a hypothetical row.
    CumeDist,
    /// `dense_rank` of a hypothetical row.
    DenseRank,
}

impl Ordered {
    /// The final function of an ordered-set aggregate by its `prosrc`.
    fn by_src(src: &str) -> Option<Ordered> {
        Some(match src {
            "percentile_disc_final" => Ordered::Disc { multi: false },
            "percentile_disc_multi_final" => Ordered::Disc { multi: true },
            "percentile_cont_float8_final" => Ordered::Cont { multi: false, interval: false },
            "percentile_cont_float8_multi_final" => Ordered::Cont { multi: true, interval: false },
            "percentile_cont_interval_final" => Ordered::Cont { multi: false, interval: true },
            "percentile_cont_interval_multi_final" => Ordered::Cont { multi: true, interval: true },
            "mode_final" => Ordered::Mode,
            "hypothetical_rank_final" => Ordered::Rank,
            "hypothetical_percent_rank_final" => Ordered::PercentRank,
            "hypothetical_cume_dist_final" => Ordered::CumeDist,
            "hypothetical_dense_rank_final" => Ordered::DenseRank,
            _ => return None,
        })
    }
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

/// `resolve_aggregate_transtype`: the actual type of a polymorphic state, from the types of the arguments of the call, or `None` when they do not give it.
fn transtype(agg: &Aggref, declared: u32) -> Option<u32> {
    let proc = builtin::proc_by_oid(agg.oid)?;
    let inputs: Vec<u32> = agg.direct.iter().chain(&agg.args[..agg.nargs]).map(|a| a.ty).collect();
    let mut args = proc.argtypes.to_vec();
    if args.len() != inputs.len() {
        return None;
    }
    rupg_analyze::resolve_polymorphic(&inputs, &mut args, declared).ok()
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
    if let Some(ordered) = Ordered::by_src(src(row.finalfn)) {
        return Ok(Method::OrderedSet(ordered));
    }
    Ok(match (src(row.transfn), src(row.finalfn)) {
        ("int8_avg_accum", "numeric_poly_sum") => Method::Int128 { avg: false },
        ("interval_avg_accum", "interval_sum") => Method::Interval { avg: false },
        ("interval_avg_accum", "interval_avg") => Method::Interval { avg: true },
        ("int8_avg_accum", "numeric_poly_avg") => Method::Int128 { avg: true },
        ("numeric_avg_accum", "numeric_sum") => Method::Numeric { avg: false },
        ("numeric_avg_accum", "numeric_avg") => Method::Numeric { avg: true },
        ("string_agg_transfn", "string_agg_finalfn") => Method::Concat { bytea: false },
        ("bytea_string_agg_transfn", "bytea_string_agg_finalfn") => Method::Concat { bytea: true },
        ("array_agg_transfn", "array_agg_finalfn") => Method::ArrayAgg,
        ("array_agg_array_transfn", "array_agg_array_finalfn") => Method::ArrayAggArray,
        ("range_agg_transfn" | "multirange_agg_transfn", "range_agg_finalfn") => Method::RangeAgg,
        (
            trans,
            "json_agg_finalfn"
            | "jsonb_agg_finalfn"
            | "json_object_agg_finalfn"
            | "jsonb_object_agg_finalfn",
        ) => {
            let (jsonb, trans) = match trans.strip_prefix("jsonb_") {
                Some(rest) => (true, rest),
                None => (false, trans.strip_prefix("json_").unwrap_or(trans)),
            };
            let (object, strict, unique) = match trans {
                "agg_transfn" => (false, false, false),
                "agg_strict_transfn" => (false, true, false),
                "object_agg_transfn" => (true, false, false),
                "object_agg_strict_transfn" => (true, true, false),
                "object_agg_unique_transfn" => (true, false, true),
                "object_agg_unique_strict_transfn" => (true, true, true),
                _ => return Err(unsupported()),
            };
            Method::Json { object, jsonb, strict, unique }
        }
        ("int2_accum" | "int4_accum" | "int8_accum" | "numeric_accum", last) => {
            let last = last.strip_prefix("numeric_").unwrap_or(last);
            let last = last.strip_prefix("poly_").unwrap_or(last);
            match last {
                "var_pop" => Method::Moments { sample: false, root: false },
                "var_samp" => Method::Moments { sample: true, root: false },
                "stddev_pop" => Method::Moments { sample: false, root: true },
                "stddev_samp" => Method::Moments { sample: true, root: true },
                _ => return Err(unsupported()),
            }
        }
        _ => {
            let state = if !polymorphic(row.transtype) {
                row.transtype
            } else if row.finalfn == 0 {
                ty
            } else {
                transtype(agg, row.transtype).ok_or_else(unsupported)?
            };
            // A state of `anyarray` holds the values of a column of that type, such as the columns of `pg_stats`, which keep their element type.
            let anyarray = state == oid::ANYARRAY && ty == oid::ANYARRAY;
            if state == oid::INTERNAL || (polymorphic(state) && !anyarray) || row.kind != b'n' {
                return Err(unsupported());
            }
            if row.initval.is_some() && !rupg_func::input_supported(state) {
                return Err(unsupported());
            }
            // A function of the catalog, as `CREATE AGGREGATE` gives, runs by its OID.
            let user = |f: u32| f >= rupg_catalog::FIRST_NORMAL_OID;
            if user(row.transfn) || user(row.finalfn) || row.finalextra {
                let callable = |f: u32| user(f) || rupg_func::kernel(f).is_some();
                if !callable(row.transfn) || (row.finalfn != 0 && !callable(row.finalfn)) {
                    return Err(unsupported());
                }
                return Ok(Method::Function {
                    trans: row.transfn,
                    trans_strict: strict(row.transfn),
                    state,
                    init: row.initval,
                    last: (row.finalfn != 0)
                        .then(|| (row.finalfn, strict(row.finalfn), row.finalextra)),
                });
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
#[derive(Clone)]
enum State {
    /// The state of [`Method::Interval`].
    Interval(rupg_func::IntervalSum),
    /// The state of [`Method::OrderedSet`], which keeps the rows of the group outside of the state.
    Sorted,
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
    /// The dimensions of the first input array, the number of input arrays and their elements.
    ArrayAggArray {
        dims: Vec<ArrayDim>,
        count: i32,
        values: Vec<Option<Value>>,
    },
    /// The input ranges. An empty multirange adds an empty range, so that the result is an empty multirange and not null.
    RangeAgg {
        ranges: Vec<Range>,
    },
    Moments(Moments),
    /// The state of [`Method::Json`]: the text so far, or `None` before the first row, and the keys of an object for `unique`.
    Json {
        text: Option<String>,
        keys: HashSet<String>,
    },
}

/// The state of [`Method::Moments`], as `NumericAggState` has it. The sums of integers are in `small` until they do not fit, and then they go into `big`.
#[derive(Clone, Default)]
struct Moments {
    count: i64,
    nan: i64,
    pinf: i64,
    ninf: i64,
    /// The sum and the sum of squares of the integers.
    small: (i128, i128),
    /// The sum and the sum of squares of the values of `numeric` and of the integers that did not fit in `small`.
    big: Option<(Numeric, Numeric)>,
}

impl Moments {
    fn add_big(&mut self, value: &Numeric, square: &Numeric) -> Result<()> {
        self.big = Some(match self.big.take() {
            Some((sum, squares)) => (
                sum.add(value).map_err(rupg_func::type_error)?,
                squares.add(square).map_err(rupg_func::type_error)?,
            ),
            None => (value.clone(), square.clone()),
        });
        Ok(())
    }

    fn add_integer(&mut self, value: i64) -> Result<()> {
        let value = i128::from(value);
        let square = value * value;
        self.count += 1;
        let (sum, squares) = self.small;
        if let (Some(sum), Some(squares)) = (sum.checked_add(value), squares.checked_add(square)) {
            self.small = (sum, squares);
            return Ok(());
        }
        self.small = (0, 0);
        self.add_big(&Numeric::from_integer(sum), &Numeric::from_integer(squares))?;
        self.add_big(&Numeric::from_integer(value), &Numeric::from_integer(square))
    }

    fn add_numeric(&mut self, value: &Numeric) -> Result<()> {
        match value.sign() {
            NumericSign::NaN => self.nan += 1,
            NumericSign::Infinity => self.pinf += 1,
            NumericSign::NegativeInfinity => self.ninf += 1,
            _ => {
                self.count += 1;
                let square = value.mul(value).map_err(rupg_func::type_error)?;
                self.add_big(value, &square)?;
            }
        }
        Ok(())
    }

    /// `numeric_stddev_internal`: null for no values and for one value of a sample, and `NaN` when a value is not finite.
    fn finish(self, sample: bool, root: bool) -> Result<Value> {
        let total = self.count + self.nan + self.pinf + self.ninf;
        if total == 0 || (sample && total <= 1) {
            return Ok(Value::Null);
        }
        if total > self.count {
            return Ok(Value::Numeric(Numeric::NAN));
        }
        let (mut sum, mut squares) =
            (Numeric::from_integer(self.small.0), Numeric::from_integer(self.small.1));
        if let Some((big_sum, big_squares)) = &self.big {
            sum = sum.add(big_sum).map_err(rupg_func::type_error)?;
            squares = squares.add(big_squares).map_err(rupg_func::type_error)?;
        }
        Numeric::moment(self.count, &sum, &squares, sample, root)
            .map(Value::Numeric)
            .map_err(rupg_func::type_error)
    }
}

/// An aggregate call of the query with its method.
struct Agg<'q> {
    agg: &'q Aggref,
    /// The address of the call in the query, which [`Eval`] finds the result by. For an aggregate of the query in a subquery, `agg` is a copy whose arguments read the query, and `key` is the call in the subquery.
    key: *const Aggref,
    ty: u32,
    method: Method,
    /// The types of the arguments that the transition function gets.
    types: Vec<u32>,
}

impl Agg<'_> {
    fn start(&self, eval: &Eval<'_>) -> Result<State> {
        Ok(match &self.method {
            Method::Kernel { state, init, .. } | Method::Function { state, init, .. } => match init
            {
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
            Method::ArrayAggArray => {
                State::ArrayAggArray { dims: Vec::new(), count: 0, values: Vec::new() }
            }
            Method::RangeAgg => State::RangeAgg { ranges: Vec::new() },
            Method::Moments { .. } => State::Moments(Moments::default()),
            Method::Interval { .. } => State::Interval(rupg_func::IntervalSum::default()),
            Method::Json { .. } => State::Json { text: None, keys: HashSet::new() },
            Method::OrderedSet(_) => State::Sorted,
        })
    }

    /// `advance_transition_function`: adds the values of the arguments of one row to the state.
    fn advance(&self, state: &mut State, args: &[Value], eval: &Eval<'_>) -> Result<()> {
        let args = &args[..self.agg.nargs];
        match (&self.method, state) {
            (
                Method::Function { trans, trans_strict, state: ty, .. },
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
                *value = eval.call(*trans, &types, *ty, false, &values)?;
                *empty = false;
            }
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
                let call = Call {
                    session: eval.session,
                    args: &types,
                    ret: *ty,
                    variadic: false,
                    want: None,
                };
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
            (Method::ArrayAggArray, State::ArrayAggArray { dims, count, values }) => {
                accumulate_array(args.first(), dims, count, values)?;
            }
            (Method::RangeAgg, State::RangeAgg { ranges }) => match args.first() {
                Some(Value::Range(range)) => ranges.push((**range).clone()),
                Some(Value::Multirange(input)) if input.is_empty() => ranges.push(Range::empty()),
                Some(Value::Multirange(input)) => ranges.extend(input.iter().cloned()),
                Some(Value::Null) | None => {}
                _ => return Err(Error::internal("range_agg got a value of the wrong kind")),
            },
            (Method::Interval { .. }, State::Interval(sum)) => match args.first() {
                Some(Value::Interval(v)) => sum.add(v)?,
                Some(Value::Null) | None => {}
                _ => return Err(Error::internal("an interval sum got a value of the wrong kind")),
            },
            (Method::Moments { .. }, State::Moments(moments)) => match args.first() {
                Some(Value::Null) | None => {}
                Some(Value::Numeric(v)) => moments.add_numeric(v)?,
                Some(v) => moments.add_integer(v.as_i64().ok_or_else(|| {
                    Error::internal("a variance of integers got a value of the wrong kind")
                })?)?,
            },
            (Method::Json { object: false, jsonb, strict, .. }, State::Json { text, .. }) => {
                let value = args.first().unwrap_or(&Value::Null);
                let ty = self.types.first().copied().unwrap_or(oid::UNKNOWN);
                let first = text.is_none();
                let out = text.get_or_insert_with(|| String::from("["));
                if *strict && value.is_null() {
                    return Ok(());
                }
                if out.len() > 1 {
                    out.push_str(", ");
                    // `json_agg_transfn` puts a new line before an array or a record that is not the first value.
                    if !first && !jsonb && !value.is_null() && rupg_func::json_structured(ty) {
                        out.push_str("\n ");
                    }
                }
                rupg_func::json_value(ty, value, false, eval.session, out)?;
            }
            (Method::Json { jsonb, strict, unique, .. }, State::Json { text, keys }) => {
                let [key, value] = args else {
                    return Err(Error::internal("json_object_agg needs two arguments"));
                };
                let out = text.get_or_insert_with(|| String::from("{ "));
                if key.is_null() {
                    return Err(if *jsonb {
                        Error::new(SqlState::INVALID_PARAMETER_VALUE, "field name must not be null")
                    } else {
                        Error::new(
                            SqlState::NULL_VALUE_NOT_ALLOWED,
                            "null value not allowed for object key",
                        )
                    });
                }
                let skip = *strict && value.is_null();
                if skip && !unique {
                    return Ok(());
                }
                let (key_ty, value_ty) = match self.types[..] {
                    [k, v, ..] => (k, v),
                    _ => (oid::UNKNOWN, oid::UNKNOWN),
                };
                let mut name = String::new();
                rupg_func::json_value(key_ty, key, true, eval.session, &mut name)?;
                if *unique && !keys.insert(name.clone()) {
                    let message = if *jsonb {
                        "duplicate JSON object key value".to_owned()
                    } else {
                        format!("duplicate JSON object key value: {name}")
                    };
                    return Err(Error::new(SqlState::DUPLICATE_JSON_OBJECT_KEY_VALUE, message));
                }
                if skip {
                    return Ok(());
                }
                if out.len() > 2 {
                    out.push_str(", ");
                }
                out.push_str(&name);
                out.push_str(" : ");
                rupg_func::json_value(value_ty, value, false, eval.session, out)?;
            }
            _ => return Err(Error::internal("an aggregate state of the wrong kind")),
        }
        Ok(())
    }

    /// The result of an ordered-set aggregate from the rows of the group, which have the values of the items of `WITHIN GROUP`, and from the values of the direct arguments.
    fn ordered_set(
        &self,
        ordered: Ordered,
        direct: &[Value],
        mut rows: Vec<Vec<Value>>,
        eval: &Eval<'_>,
    ) -> Result<Value> {
        let types: Vec<u32> = self.agg.args.iter().map(|a| a.ty).collect();
        let keys = sort::Keys::new(&self.agg.order, &types, eval.session)?;
        let hypothetical = matches!(
            ordered,
            Ordered::Rank | Ordered::PercentRank | Ordered::CumeDist | Ordered::DenseRank
        );
        if hypothetical {
            return hypothetical_rank(ordered, direct, rows, &keys);
        }
        let percentile = direct.first().unwrap_or(&Value::Null);
        let multi =
            matches!(ordered, Ordered::Disc { multi: true } | Ordered::Cont { multi: true, .. });
        if !multi && ordered != Ordered::Mode {
            // `percentile_disc_final` and `percentile_cont_final_common` check the percentile first.
            let Some(p) = float(percentile) else { return Ok(Value::Null) };
            rupg_func::check_percentile(p)?;
        }
        // `ordered_set_transition` skips the null values.
        rows.retain(|r| !r[0].is_null());
        if rows.is_empty() {
            return Ok(Value::Null);
        }
        sort::qsort(&mut rows, &mut |a, b| keys.compare(a, b))?;
        let values: Vec<Value> = rows.into_iter().map(|mut r| r.swap_remove(0)).collect();
        let count = values.len();
        let interval = matches!(ordered, Ordered::Cont { interval: true, .. });
        match ordered {
            Ordered::Mode => {
                let equal = |a: &Value, b: &Value| {
                    keys.equal(std::slice::from_ref(a), std::slice::from_ref(b))
                };
                let (mut mode, mut mode_freq) = (0, 0);
                let (mut last, mut last_freq) = (0, 0);
                for i in 0..count {
                    if last_freq > 0 && equal(&values[i], &values[last])? {
                        last_freq += 1;
                    } else {
                        last = i;
                        last_freq = 1;
                    }
                    if last_freq > mode_freq {
                        mode = last;
                        mode_freq = last_freq;
                    }
                }
                Ok(values[mode].clone())
            }
            Ordered::Disc { multi: false } => {
                let p = float(percentile).unwrap_or(0.0);
                Ok(values[disc_row(p, count) - 1].clone())
            }
            Ordered::Cont { multi: false, .. } => {
                let p = float(percentile).unwrap_or(0.0);
                cont(&values, p, interval)
            }
            _ => {
                let Value::Array(array) = percentile else { return Ok(Value::Null) };
                if array.values.is_empty() {
                    return Ok(Value::Array(Box::new(Array::empty())));
                }
                let percentiles = array
                    .values
                    .iter()
                    .map(|v| {
                        let p = v.as_ref().and_then(float);
                        if let Some(p) = p {
                            rupg_func::check_percentile(p)?;
                        }
                        Ok(p)
                    })
                    .collect::<Result<Vec<_>>>()?;
                let result = percentiles
                    .into_iter()
                    .map(|p| {
                        let Some(p) = p else { return Ok(None) };
                        if matches!(ordered, Ordered::Disc { .. }) {
                            Ok(Some(values[disc_row(p, count) - 1].clone()))
                        } else {
                            cont(&values, p, interval).map(Some)
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Value::Array(Box::new(Array { dims: array.dims.clone(), values: result })))
            }
        }
    }

    /// `finalize_aggregate`: the result of the aggregate from its state.
    fn finish(&self, state: State, eval: &Eval<'_>) -> Result<Value> {
        let numeric = |v: i128| Numeric::from_integer(v);
        let divide = |sum: Numeric, count: i64| {
            sum.div(&numeric(i128::from(count))).map(Value::Numeric).map_err(rupg_func::type_error)
        };
        match (&self.method, state) {
            (Method::Function { last, state: ty, .. }, State::Value { value, .. }) => match last {
                None => Ok(value),
                Some((_, true, _)) if value.is_null() => Ok(Value::Null),
                Some((f, _, extra)) => {
                    let mut types = vec![*ty];
                    let mut values = vec![value];
                    if *extra {
                        types.extend(&self.types);
                        values.resize(types.len(), Value::Null);
                    }
                    eval.call(*f, &types, self.ty, false, &values)
                }
            },
            (Method::Kernel { last, state: ty, .. }, State::Value { value, .. }) => match last {
                None => Ok(value),
                Some((_, true)) if value.is_null() => Ok(Value::Null),
                Some((f, _)) => {
                    let types = [*ty];
                    let call = Call {
                        session: eval.session,
                        args: &types,
                        ret: self.ty,
                        variadic: false,
                        want: None,
                    };
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
            (Method::ArrayAggArray, State::ArrayAggArray { dims, count, values }) => {
                if count == 0 {
                    return Ok(Value::Null);
                }
                let mut all = Vec::with_capacity(dims.len() + 1);
                all.push(ArrayDim { len: count, lower: 1 });
                all.extend(dims);
                Ok(Value::Array(Box::new(Array { dims: all, values })))
            }
            (Method::RangeAgg, State::RangeAgg { ranges }) => rupg_func::range_agg(self.ty, ranges),
            (Method::Interval { avg }, State::Interval(sum)) => {
                let result = if *avg { sum.avg()? } else { sum.sum()? };
                Ok(result.map_or(Value::Null, Value::Interval))
            }
            (Method::Moments { sample, root }, State::Moments(moments)) => {
                moments.finish(*sample, *root)
            }
            (Method::Json { object, jsonb, .. }, State::Json { text, .. }) => {
                let Some(mut text) = text else { return Ok(Value::Null) };
                text.push_str(if *object { " }" } else { "]" });
                if *jsonb {
                    rupg_types::jsonb_in(&text).map(Value::Text).map_err(rupg_func::type_error)
                } else {
                    Ok(Value::Text(text))
                }
            }
            _ => Err(Error::internal("an aggregate state of the wrong kind")),
        }
    }
}

/// `accumulateArrayResultArr`: adds the elements of one input array of `array_agg` to the state. Each input array must have the dimensions and the lower bounds of the first input array.
pub(crate) fn accumulate_array(
    input: Option<&Value>,
    dims: &mut Vec<ArrayDim>,
    count: &mut i32,
    values: &mut Vec<Option<Value>>,
) -> Result<()> {
    let subscript = |message: &str| Error::new(SqlState::ARRAY_SUBSCRIPT_ERROR, message);
    let array = match input {
        Some(Value::Array(array)) => array,
        Some(Value::Null) | None => {
            return Err(Error::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "cannot accumulate null arrays",
            ));
        }
        Some(_) => return Err(Error::internal("array_agg got a value that is not an array")),
    };
    if *count == 0 {
        if array.dims.is_empty() {
            return Err(subscript("cannot accumulate empty arrays"));
        }
        if array.dims.len() + 1 > MAXDIM {
            return Err(Error::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!(
                    "number of array dimensions ({}) exceeds the maximum allowed ({MAXDIM})",
                    array.dims.len() + 1
                ),
            ));
        }
        dims.clone_from(&array.dims);
    } else if array.dims != *dims {
        return Err(subscript("cannot accumulate arrays of different dimensionality"));
    }
    *count = count
        .checked_add(1)
        .ok_or_else(|| Error::internal("array_agg has too many input arrays"))?;
    values.extend(array.values.iter().cloned());
    Ok(())
}

/// The value of a `double precision` percentile, or `None` for null.
fn float(value: &Value) -> Option<f64> {
    match value {
        Value::Float8(p) => Some(*p),
        _ => None,
    }
}

/// The row of `percentile_disc` from 1: the smallest K such that K / N is at least the percentile, and at least 1.
fn disc_row(p: f64, count: usize) -> usize {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let row = (p * count as f64).ceil() as usize;
    row.clamp(1, count)
}

/// `percentile_cont` of sorted values with no nulls: the value at the fraction `p` of the way from the first value to the last, by `float8_lerp` or `interval_lerp`.
fn cont(values: &[Value], p: f64, interval: bool) -> Result<Value> {
    #[allow(clippy::cast_precision_loss)]
    let at = p * (values.len() - 1) as f64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (first, second) = (at.floor() as usize, at.ceil() as usize);
    if first == second {
        return Ok(values[first].clone());
    }
    let proportion = at - at.floor();
    match (&values[first], &values[second]) {
        (Value::Float8(lo), Value::Float8(hi)) if !interval => {
            Ok(Value::Float8(lo + proportion * (hi - lo)))
        }
        (Value::Interval(lo), Value::Interval(hi)) if interval => {
            rupg_func::interval_lerp(lo, hi, proportion).map(Value::Interval)
        }
        _ => Err(Error::internal("percentile_cont got a value of the wrong type")),
    }
}

/// `hypothetical_rank_common` and `hypothetical_dense_rank_final`: the rank of the hypothetical row of the direct arguments among the rows of the group. The hypothetical row comes before the rows that are equal to it, and after them for `cume_dist`.
fn hypothetical_rank(
    ordered: Ordered,
    direct: &[Value],
    rows: Vec<Vec<Value>>,
    keys: &sort::Keys<'_>,
) -> Result<Value> {
    let count = rows.len();
    if count == 0 {
        return Ok(match ordered {
            Ordered::PercentRank => Value::Float8(0.0),
            Ordered::CumeDist => Value::Float8(1.0),
            _ => Value::Int8(1),
        });
    }
    let hypothetical = &direct[direct.len() - rows[0].len()..];
    let mut rank = 1_usize;
    if ordered == Ordered::DenseRank {
        let mut before = Vec::new();
        for row in rows {
            if keys.compare(&row, hypothetical)?.is_lt() {
                before.push(row);
            }
        }
        let mut rows = before;
        sort::qsort(&mut rows, &mut |a, b| keys.compare(a, b))?;
        for (i, row) in rows.iter().enumerate() {
            if i == 0 || !keys.equal(&rows[i - 1], row)? {
                rank += 1;
            }
        }
    } else {
        for row in &rows {
            let order = keys.compare(row, hypothetical)?;
            if order.is_lt() || (ordered == Ordered::CumeDist && order.is_eq()) {
                rank += 1;
            }
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let (rank_f, count_f) = (rank as f64, count as f64);
    Ok(match ordered {
        Ordered::PercentRank => Value::Float8((rank_f - 1.0) / count_f),
        Ordered::CumeDist => Value::Float8(rank_f / (count_f + 1.0)),
        _ => Value::Int8(i64::try_from(rank).unwrap_or(i64::MAX)),
    })
}

/// The aggregate call of a call of an aggregate function with `OVER`, as the executor reads it.
pub(crate) fn window_aggref(call: &WindowFunc) -> Aggref {
    Aggref {
        oid: call.oid,
        kind: b'n',
        direct: Vec::new(),
        args: call.args.clone(),
        nargs: call.args.len(),
        order: Vec::new(),
        distinct: Vec::new(),
        star: call.star,
        variadic: false,
        filter: call.filter.clone(),
        levels_up: 0,
    }
}

/// `initialize_peragg`: the method of the moving-aggregate mode of an aggregate call with `OVER`, with its inverse transition function and true when that function is strict, when the window uses the mode. It uses the mode when the aggregate has an inverse transition function and the start of the frame can move, or when only the final function of the mode leaves the state as it is, and the arguments and the filter of the call have no volatile function and no subquery. The engine runs the mode for an aggregate of `CREATE AGGREGATE` with a function of the catalog, which it calls by its OID. A built-in aggregate gives the same results in both modes.
fn moving_method(
    call: &WindowFunc,
    aggref: &Aggref,
    ty: u32,
    frame: &Frame,
) -> Result<Option<(Method, u32, bool)>> {
    let Some(row) = builtin::aggregate(call.oid) else { return Ok(None) };
    if row.minvtransfn == 0 {
        return Ok(None);
    }
    let forced = row.mfinalmodify == b'r' && row.finalmodify != b'r';
    let unsafe_expr = |e: &Expr| {
        crate::volatile(e)
            || e.find(0, &mut |e, _| matches!(e.kind, ExprKind::SubLink(_)).then_some(())).is_some()
    };
    let moves = frame.start_bound() != FrameBound::UnboundedPreceding
        && !call.args.iter().chain(call.filter.as_deref()).any(unsafe_expr);
    if !forced && !moves {
        return Ok(None);
    }
    let user = |f: u32| f >= rupg_catalog::FIRST_NORMAL_OID;
    if ![row.mtransfn, row.minvtransfn, row.mfinalfn].into_iter().any(user) {
        return Ok(None);
    }
    let unsupported = || not_yet(format!("aggregate function {}", signature(call.oid)));
    let callable = |f: u32| user(f) || rupg_func::kernel(f).is_some();
    if !callable(row.mtransfn)
        || !callable(row.minvtransfn)
        || (row.mfinalfn != 0 && !callable(row.mfinalfn))
    {
        return Err(unsupported());
    }
    let state = if !polymorphic(row.mtranstype) {
        row.mtranstype
    } else if row.mfinalfn == 0 {
        ty
    } else {
        transtype(aggref, row.mtranstype).ok_or_else(unsupported)?
    };
    if state == oid::INTERNAL
        || polymorphic(state)
        || (row.minitval.is_some() && !rupg_func::input_supported(state))
    {
        return Err(unsupported());
    }
    let method = Method::Function {
        trans: row.mtransfn,
        trans_strict: strict(row.mtransfn),
        state,
        init: row.minitval,
        last: (row.mfinalfn != 0).then(|| (row.mfinalfn, strict(row.mfinalfn), row.mfinalextra)),
    };
    Ok(Some((method, row.minvtransfn, strict(row.minvtransfn))))
}

/// `eval_windowaggregates`: the value of an aggregate function with `OVER` for each row of one partition, which is the result of the rows of the frame of the row. `frames` has the frame of each row, and `frame` is the clause of the window.
pub(crate) fn window_values(
    eval: &Eval<'_>,
    call: &WindowFunc,
    ty: u32,
    part: &[usize],
    frames: &[window::Bounds],
    frame: &Frame,
    input: window::Input<'_>,
) -> Result<Vec<Value>> {
    let aggref = window_aggref(call);
    let moving = moving_method(call, &aggref, ty, frame)?;
    let (method, inverse) = match moving {
        Some((method, inv, inv_strict)) => (method, Some((inv, inv_strict))),
        None => (method(&aggref, ty, None)?, None),
    };
    let agg = Agg {
        agg: &aggref,
        key: std::ptr::from_ref(&aggref),
        ty,
        method,
        types: aggref.args.iter().map(|a| a.ty).collect(),
    };
    if let Some(inverse) = inverse {
        return moving_values(eval, call, &agg, inverse, part, frames, input);
    }
    let feed = |state: &mut State, at: usize| -> Result<()> {
        // The arguments of the call, and not their copies in `aggref`, have the aggregate calls that the group computed.
        let mut e = input.at(eval, part[at]);
        if let Some(filter) = &call.filter
            && e.eval(filter)? != Value::Bool(true)
        {
            return Ok(());
        }
        let args = call.args.iter().map(|a| e.eval(a)).collect::<Result<Vec<_>>>()?;
        agg.advance(state, &args, eval)
    };
    // The state has the rows of the partition from `base` to `upto`. A frame with the same start and more rows at the end adds the new rows, as `eval_windowaggregates` does it. A frame with an other start, or with an exclusion, starts again.
    let mut state = agg.start(eval)?;
    let (mut base, mut upto) = (0, 0);
    let mut out: Vec<Value> = Vec::with_capacity(part.len());
    let mut last: Option<&window::Bounds> = None;
    for bounds in frames {
        if let (Some(previous), Some(value)) = (last, out.last())
            && previous == bounds
            && !bounds.excludes()
        {
            out.push(value.clone());
            continue;
        }
        last = Some(bounds);
        let value = if bounds.excludes() {
            let mut fresh = agg.start(eval)?;
            for at in bounds.rows() {
                feed(&mut fresh, at)?;
            }
            agg.finish(fresh, eval)?
        } else {
            if bounds.head != base || bounds.tail < upto {
                state = agg.start(eval)?;
                (base, upto) = (bounds.head, bounds.head);
            }
            for at in upto..bounds.tail {
                feed(&mut state, at)?;
            }
            upto = upto.max(bounds.tail);
            agg.finish(state.clone(), eval)?
        };
        out.push(value);
    }
    Ok(out)
}

/// `eval_windowaggregates` in the moving-aggregate mode: when the start of the frame moves, `advance_windowaggregate_base` removes each row that leaves the frame with the inverse transition function `inverse`, and the aggregate starts again when that function gives null. The state of the mode is never null after a row, so a transition function that gives null is an error. The state keeps the count of its rows, and the removal of the last row starts the aggregate again in place of a call of the inverse function.
fn moving_values(
    eval: &Eval<'_>,
    call: &WindowFunc,
    agg: &Agg<'_>,
    (inverse, inverse_strict): (u32, bool),
    part: &[usize],
    frames: &[window::Bounds],
    input: window::Input<'_>,
) -> Result<Vec<Value>> {
    let Method::Function { trans_strict, state: state_ty, .. } = agg.method else {
        return Err(Error::internal("a moving aggregate with no function"));
    };
    let nargs = agg.agg.nargs;
    // The arguments of the call on a row of the partition, or `None` when the filter of the call skips the row.
    let args_at = |at: usize| -> Result<Option<Vec<Value>>> {
        let mut e = input.at(eval, part[at]);
        if let Some(filter) = &call.filter
            && e.eval(filter)? != Value::Bool(true)
        {
            return Ok(None);
        }
        call.args.iter().map(|a| e.eval(a)).collect::<Result<Vec<_>>>().map(Some)
    };
    let add = |state: &mut State, count: &mut usize, at: usize| -> Result<()> {
        let Some(args) = args_at(at)? else { return Ok(()) };
        if trans_strict && args[..nargs].iter().any(Value::is_null) {
            return Ok(());
        }
        agg.advance(state, &args, eval)?;
        if let State::Value { value: Value::Null, .. } = state {
            return Err(Error::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "moving-aggregate transition function must not return null",
            ));
        }
        *count += 1;
        Ok(())
    };
    // `advance_windowaggregate_base`: false when the inverse function gives null.
    let remove = |state: &mut State, count: &mut usize, at: usize| -> Result<bool> {
        let Some(args) = args_at(at)? else { return Ok(true) };
        let args = &args[..nargs];
        if inverse_strict && args.iter().any(Value::is_null) {
            return Ok(true);
        }
        let State::Value { value, .. } = state else {
            return Err(Error::internal("a moving aggregate with no value"));
        };
        if value.is_null() {
            return Err(Error::internal(
                "aggregate transition value is NULL before inverse transition",
            ));
        }
        if *count == 1 {
            *state = agg.start(eval)?;
            *count = 0;
            return Ok(true);
        }
        let mut types = Vec::with_capacity(args.len() + 1);
        types.push(state_ty);
        types.extend(&agg.types);
        let mut values = Vec::with_capacity(args.len() + 1);
        values.push(value.clone());
        values.extend_from_slice(args);
        let result = eval.call(inverse, &types, state_ty, false, &values)?;
        if result.is_null() {
            return Ok(false);
        }
        *value = result;
        *count -= 1;
        Ok(true)
    };
    let mut state = agg.start(eval)?;
    let mut count = 0;
    let (mut base, mut upto) = (0, 0);
    let mut out: Vec<Value> = Vec::with_capacity(part.len());
    let mut last: Option<&window::Bounds> = None;
    for (row, bounds) in frames.iter().enumerate() {
        if let (Some(previous), Some(value)) = (last, out.last())
            && previous == bounds
            && !bounds.excludes()
        {
            out.push(value.clone());
            continue;
        }
        last = Some(bounds);
        let mut restart =
            row == 0 || bounds.excludes() || upto <= bounds.head || bounds.tail < upto;
        while !restart && base < bounds.head {
            restart = !remove(&mut state, &mut count, base)?;
            base += 1;
        }
        base = bounds.head;
        if restart {
            state = agg.start(eval)?;
            count = 0;
            upto = bounds.head;
        }
        for at in bounds.rows().filter(|&at| at >= upto) {
            add(&mut state, &mut count, at)?;
        }
        upto = upto.max(bounds.tail);
        out.push(agg.finish(state.clone(), eval)?);
    }
    Ok(out)
}

/// The aggregate calls of an expression. An aggregate call has no other aggregate call in it. An aggregate of a query outside the query is not a call of the query.
fn collect<'q>(expr: &'q Expr, out: &mut Vec<(&'q Aggref, &'q Expr)>) {
    if let ExprKind::Agg(agg) = &expr.kind {
        if agg.levels_up == 0 {
            out.push((agg, expr));
        }
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

/// The aggregate calls of the query in the subqueries of the select list and of `HAVING`, as `replace_outer_agg` finds them, each with the number of levels between the call and the query.
fn outer_calls(query: &Query) -> Vec<(&Aggref, &Expr, usize)> {
    let mut out = Vec::new();
    for expr in query.targets.iter().map(|t| &t.expr).chain(&query.having) {
        expr.find(0, &mut |e, depth| {
            if let ExprKind::Agg(agg) = &e.kind
                && depth > 0
                && agg.levels_up == depth
            {
                out.push((&**agg, e, depth));
            }
            None::<()>
        });
    }
    out
}

/// Checks that the engine has every aggregate and every function of the groups.
pub(crate) fn check(query: &Query) -> Result<()> {
    let outer = outer_calls(query).into_iter().map(|(agg, expr, _)| (agg, expr));
    for (agg, expr) in calls(query).into_iter().chain(outer) {
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

/// The groups of the rows on the items of `GROUP BY` of a grouping set, as the indexes of the rows of each group. `set` has the targets of the items.
fn groups<'a>(
    eval: &mut Eval<'a>,
    query: &Query,
    set: &[usize],
    tuples: &'a [Vec<usize>],
) -> Result<Vec<Vec<usize>>> {
    let items: Vec<&SortGroup> = query.group.iter().filter(|g| set.contains(&g.target)).collect();
    if items.is_empty() {
        return Ok(vec![(0..tuples.len()).collect()]);
    }
    let exprs: Vec<&Expr> = items.iter().map(|g| &query.targets[g.target].expr).collect();
    let mut keys = Vec::with_capacity(tuples.len());
    for tuple in tuples {
        eval.tuple = tuple;
        keys.push(exprs.iter().map(|e| eval.eval(e)).collect::<Result<Vec<_>>>()?);
    }
    let list: Vec<SortGroup> =
        items.iter().enumerate().map(|(i, g)| SortGroup { target: i, ..(*g).clone() }).collect();
    let types: Vec<u32> = exprs.iter().map(|e| e.ty).collect();
    let keys_of = sort::Keys::new(&list, &types, eval.session)?;
    if let Some(groups) = hashed_groups(eval, query, &items, &types, &keys, &keys_of)? {
        return Ok(groups);
    }
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

/// The groups of grouping sets, each with the place of its set.
type SetGroups = Vec<(usize, Vec<usize>)>;

/// The sets of the passes of the `Agg` of the grouping sets of the plan, and the groups of each set with the place of its set, in the order that the node gives them: the passes over sorted rows, then the hash tables. `None` when the plan has no such `Agg`, when its sets are not `sets`, or when a pass needs a sort or a hash that a key has no function for.
fn passes<'a>(
    eval: &mut Eval<'a>,
    query: &Query,
    sets: &[Vec<usize>],
    tuples: &'a [Vec<usize>],
) -> Result<Option<(Vec<Vec<usize>>, SetGroups)>> {
    if query.grouping_sets.is_empty() {
        return Ok(None);
    }
    let Some(tables) = eval.session.hash_tables(query) else {
        return Ok(None);
    };
    let sorted = |s: &Vec<usize>| {
        let mut s = s.clone();
        s.sort_unstable();
        s
    };
    let mut wanted: Vec<Vec<usize>> = sets.iter().map(sorted).collect();
    let mut found: Vec<Vec<usize>> =
        tables.passes.iter().flat_map(|p| p.sets.iter().map(sorted)).collect();
    wanted.sort();
    found.sort();
    if wanted.is_empty() || wanted != found {
        return Ok(None);
    }
    let mut all_sets = Vec::new();
    let mut out = Vec::new();
    for pass in &tables.passes {
        let base = all_sets.len();
        let groups = if pass.hashed {
            let ([set], [groups]) = (&pass.sets[..], &pass.groups[..]) else {
                return Ok(None);
            };
            hashed_pass(eval, query, set, *groups, tuples)?
                .map(|g| g.into_iter().map(|g| (0, g)).collect())
        } else {
            sorted_pass(eval, query, &pass.sets, tuples)?
        };
        let Some(groups) = groups else {
            return Ok(None);
        };
        out.extend(groups.into_iter().map(|(s, g)| (base + s, g)));
        all_sets.extend(pass.sets.iter().cloned());
    }
    Ok(Some((all_sets, out)))
}

/// The keys of `GROUP BY` of the targets `set` and their values for each row, or `None` when a target is not an item of `GROUP BY`.
#[allow(clippy::type_complexity)]
fn set_keys<'a>(
    eval: &mut Eval<'a>,
    query: &Query,
    set: &[usize],
    tuples: &'a [Vec<usize>],
) -> Result<Option<(Vec<SortGroup>, Vec<u32>, Vec<Vec<Value>>)>> {
    let Some(items) =
        set.iter().map(|t| query.group.iter().find(|g| g.target == *t)).collect::<Option<Vec<_>>>()
    else {
        return Ok(None);
    };
    let exprs: Vec<&Expr> = items.iter().map(|g| &query.targets[g.target].expr).collect();
    let mut keys = Vec::with_capacity(tuples.len());
    for tuple in tuples {
        eval.tuple = tuple;
        keys.push(exprs.iter().map(|e| eval.eval(e)).collect::<Result<Vec<_>>>()?);
    }
    let list =
        items.iter().enumerate().map(|(i, g)| SortGroup { target: i, ..(*g).clone() }).collect();
    Ok(Some((list, exprs.iter().map(|e| e.ty).collect(), keys)))
}

/// The groups of a pass over the rows in the order of the keys of the first set, as `agg_retrieve_direct` gives them: at the end of each group of the first set, that group and then the group of each smaller set that ends with the same row. The groups of the sets with no keys end with the last row, and they come also when there are no rows. `None` when a set is not the first keys of the set before it or a key has no sort operator.
fn sorted_pass<'a>(
    eval: &mut Eval<'a>,
    query: &Query,
    sets: &[Vec<usize>],
    tuples: &'a [Vec<usize>],
) -> Result<Option<SetGroups>> {
    let first = &sets[0];
    if sets.iter().any(|s| s.len() > first.len() || s[..] != first[..s.len()]) {
        return Ok(None);
    }
    let Some((list, types, keys)) = set_keys(eval, query, first, tuples)? else {
        return Ok(None);
    };
    if list.iter().any(|g| g.sort == 0) {
        return Ok(None);
    }
    let keys_of = sort::Keys::new(&list, &types, eval.session)?;
    // The keys of the first items of the list, by the number of the items.
    let prefixes = (0..=list.len())
        .map(|n| sort::Keys::new(&list[..n], &types, eval.session))
        .collect::<Result<Vec<_>>>()?;
    let mut order: Vec<usize> = (0..tuples.len()).collect();
    sort::qsort(&mut order, &mut |a, b| Ok(keys_of.compare(&keys[*a], &keys[*b])?.then(a.cmp(b))))?;
    let mut out = Vec::new();
    let mut open: Vec<Vec<usize>> = vec![Vec::new(); sets.len()];
    for (n, &row) in order.iter().enumerate() {
        for group in &mut open {
            group.push(row);
        }
        let next = order.get(n + 1);
        for (s, set) in sets.iter().enumerate() {
            let ends = match next {
                None => true,
                Some(&next) => !prefixes[set.len()].equal(&keys[row], &keys[next])?,
            };
            if ends {
                out.push((s, std::mem::take(&mut open[s])));
            }
        }
    }
    if tuples.is_empty() {
        out.extend(
            sets.iter().enumerate().filter(|(_, s)| s.is_empty()).map(|(s, _)| (s, Vec::new())),
        );
    }
    Ok(Some(out))
}

/// The groups of the hash table of a set of an `Agg` with grouping sets for `groups` groups, in the order of the table, or `None` when a key has no hash function.
fn hashed_pass<'a>(
    eval: &mut Eval<'a>,
    query: &Query,
    set: &[usize],
    groups: f64,
    tuples: &'a [Vec<usize>],
) -> Result<Option<Vec<Vec<usize>>>> {
    let Some((list, types, keys)) = set_keys(eval, query, set, tuples)? else {
        return Ok(None);
    };
    let keys_of = sort::Keys::new(&list, &types, eval.session)?;
    let mut hash = |row: usize| hashed::hash(types.iter().copied().zip(&keys[row]));
    if keys.first().is_some_and(|_| hash(0).is_err()) {
        return Ok(None);
    }
    let groups = hashed::groups(keys.len(), groups, &mut hash, &mut |a, b| {
        keys_of.equal(&keys[a], &keys[b])
    })?;
    Ok(Some(groups))
}

/// The groups of the rows in the order of the hash table of the `HashAggregate` of the plan, or `None` when the plan sorts the rows for the groups, has grouping sets, or hashes a key whose type has no hash function. `keys` has the values of `items` for each row.
fn hashed_groups(
    eval: &Eval<'_>,
    query: &Query,
    items: &[&SortGroup],
    types: &[u32],
    keys: &[Vec<Value>],
    keys_of: &sort::Keys<'_>,
) -> Result<Option<Vec<Vec<usize>>>> {
    if !query.grouping_sets.is_empty() {
        return Ok(None);
    }
    let Some(table) = eval.session.hash_tables(query).and_then(|t| t.group.clone()) else {
        return Ok(None);
    };
    // The places in `items` of the keys of the table.
    let Some(places) = table
        .keys
        .iter()
        .map(|t| items.iter().position(|g| g.target == *t))
        .collect::<Option<Vec<usize>>>()
    else {
        return Ok(None);
    };
    let mut hash = |row: usize| hashed::hash(places.iter().map(|&p| (types[p], &keys[row][p])));
    if keys.first().is_some_and(|_| hash(0).is_err()) {
        return Ok(None);
    }
    let groups = hashed::groups(keys.len(), table.groups, &mut hash, &mut |a, b| {
        keys_of.equal(&keys[a], &keys[b])
    })?;
    Ok(Some(groups))
}

/// The expressions of the select list and of `HAVING` that are items of `GROUP BY`, also in the subqueries, each with the target of its item, as `substitute_grouped_columns` finds them. They are null in the groups of a grouping set that does not have the item.
fn grouped_exprs(query: &Query) -> Vec<(*const Expr, usize)> {
    /// The items of `GROUP BY` as a subquery `depth` levels down sees them.
    struct Items<'q> {
        items: Vec<(usize, &'q Expr)>,
        raised: Vec<Vec<(usize, Expr)>>,
    }
    impl Items<'_> {
        fn find(&mut self, expr: &Expr, depth: usize) -> Option<usize> {
            if depth == 0 {
                return self.items.iter().find(|(_, g)| g.same(expr)).map(|(t, _)| *t);
            }
            while self.raised.len() < depth {
                let levels = self.raised.len() + 1;
                let raised = self
                    .items
                    .iter()
                    .map(|(t, g)| {
                        let mut g = (*g).clone();
                        g.raise(levels);
                        (*t, g)
                    })
                    .collect();
                self.raised.push(raised);
            }
            self.raised[depth - 1].iter().find(|(_, g)| g.same(expr)).map(|(t, _)| *t)
        }

        fn walk(&mut self, expr: &Expr, depth: usize, out: &mut Vec<(*const Expr, usize)>) {
            if let Some(target) = self.find(expr, depth) {
                out.push((std::ptr::from_ref(expr), target));
                return;
            }
            match &expr.kind {
                // The arguments of an aggregate of the query read the rows of the group, but the direct arguments read the group.
                ExprKind::Agg(agg) if agg.levels_up == depth => {
                    for e in &agg.direct {
                        self.walk(e, depth, out);
                    }
                    return;
                }
                ExprKind::Agg(agg) if agg.levels_up > depth => return,
                ExprKind::Grouping(g) if g.levels_up >= depth => return,
                _ => {}
            }
            for child in expr.children() {
                self.walk(child, depth, out);
            }
            if let ExprKind::SubLink(sub) = &expr.kind {
                for (e, depth) in sub.query.all_exprs(depth + 1) {
                    self.walk(e, depth, out);
                }
            }
        }
    }
    let items = query.group.iter().map(|g| (g.target, &query.targets[g.target].expr)).collect();
    let mut items = Items { items, raised: Vec::new() };
    let mut out = Vec::new();
    for (i, target) in query.targets.iter().enumerate() {
        // A target that is an item is the column of its own item, as `ressortgroupref` finds it, also when another item has the same expression.
        if query.group.iter().any(|g| g.target == i) {
            out.push((std::ptr::from_ref(&target.expr), i));
        } else {
            items.walk(&target.expr, 0, &mut out);
        }
    }
    if let Some(having) = &query.having {
        items.walk(having, 0, &mut out);
    }
    out
}

/// The number of the groups of a query before `HAVING`, which `EXPLAIN ANALYZE` shows with the groups that `HAVING` removes.
pub(crate) fn group_count<'a>(
    eval: &mut Eval<'a>,
    query: &Query,
    tuples: &'a [Vec<usize>],
) -> Result<usize> {
    let sets = if query.grouping_sets.is_empty() {
        vec![query.group.iter().map(|g| g.target).collect()]
    } else {
        expand_grouping_sets(&query.grouping_sets, query.group_distinct, usize::MAX)
            .unwrap_or_default()
    };
    let mut count = 0;
    for set in &sets {
        count += groups(eval, query, set, tuples)?.len();
    }
    Ok(count)
}

/// The rows of a query with `GROUP BY`, `HAVING` or an aggregate call: one row for each group that `HAVING` keeps, with all the items of the select list. With grouping sets, each set makes its own groups, in the order that the `Agg` of the plan gives them, and else with the sets with more items first.
pub(crate) fn rows<'a>(
    eval: &mut Eval<'a>,
    query: &'a Query,
    tuples: &'a [Vec<usize>],
    none: &'a [usize],
) -> Result<Vec<Vec<Value>>> {
    let outer = outer_calls(query);
    let lowered: Vec<Aggref> = outer
        .iter()
        .map(|(agg, _, depth)| {
            let mut copy = (*agg).clone();
            for e in copy.direct.iter_mut().chain(&mut copy.args).chain(copy.filter.as_deref_mut())
            {
                e.lower(*depth);
            }
            copy.levels_up = 0;
            copy
        })
        .collect();
    let all =
        calls(query).into_iter().map(|(agg, expr)| (agg, std::ptr::from_ref(agg), expr)).chain(
            lowered
                .iter()
                .zip(&outer)
                .map(|(copy, (agg, expr, _))| (copy, std::ptr::from_ref(*agg), *expr)),
        );
    let aggs: Vec<Agg<'_>> = all
        .map(|(agg, key, expr)| {
            Ok(Agg {
                agg,
                key,
                ty: expr.ty,
                method: method(agg, expr.ty, expr.location)?,
                types: agg.args[..agg.nargs].iter().map(|a| a.ty).collect(),
            })
        })
        .collect::<Result<_>>()?;
    let levels = super::set_levels(&query.targets);
    let calls = window::calls(query);
    // The targets of the items of each grouping set.
    let all_items: Vec<usize> = query.group.iter().map(|g| g.target).collect();
    let (sets, grouped) = if query.grouping_sets.is_empty() {
        (vec![all_items.clone()], Vec::new())
    } else {
        let mut sets = expand_grouping_sets(&query.grouping_sets, query.group_distinct, usize::MAX)
            .unwrap_or_default();
        sets.reverse();
        (sets, grouped_exprs(query))
    };
    let inherited = std::mem::take(&mut eval.group.nulled);
    eval.group = Group { nulled: inherited.clone(), ..Group::default() };
    let (sets, ordered) = match passes(eval, query, &sets, tuples)? {
        Some(passes) => passes,
        None => {
            let mut ordered = Vec::new();
            for (s, set) in sets.iter().enumerate() {
                ordered.extend(groups(eval, query, set, tuples)?.into_iter().map(|g| (s, g)));
            }
            (sets, ordered)
        }
    };
    // The items that each set does not have, and the expressions that are null in its groups.
    let nulls: Vec<(Vec<usize>, Vec<*const Expr>)> = sets
        .iter()
        .map(|set| {
            let out: Vec<usize> = all_items.iter().copied().filter(|t| !set.contains(t)).collect();
            let mut nulled = inherited.clone();
            nulled.extend(grouped.iter().filter(|(_, t)| out.contains(t)).map(|(e, _)| *e));
            (out, nulled)
        })
        .collect();
    // With window function calls, the windows read the groups that `HAVING` keeps: the first row of each group and the results of its aggregate calls.
    let mut kept: Vec<(&[usize], Group)> = Vec::new();
    let mut rows = Vec::new();
    for (s, group) in ordered {
        let (out, nulled) = &nulls[s];
        rows_of_group(
            eval,
            query,
            &aggs,
            &group,
            tuples,
            none,
            Group { aggs: Vec::new(), out: out.clone(), nulled: nulled.clone() },
            &levels,
            !calls.is_empty(),
            &mut kept,
            &mut rows,
        )?;
    }
    if !calls.is_empty() {
        let (firsts, mut groups): (Vec<&[usize]>, Vec<_>) = kept.into_iter().unzip();
        let input = window::Input { tuples: &firsts, groups: &groups };
        let (sequence, mut values) = window::compute(eval, query, &calls, input, eval.session)?;
        for i in sequence {
            eval.tuple = firsts[i];
            eval.group = std::mem::take(&mut groups[i]);
            eval.windows = std::mem::take(&mut values[i]);
            eval.project(&query.targets, &levels, &mut rows, usize::MAX)?;
        }
        eval.windows.clear();
    }
    eval.group = Group { nulled: inherited, ..Group::default() };
    Ok(rows)
}

/// The aggregate calls of a group, then `HAVING` and the row of the group. With window function calls, the group goes to `kept` for the windows. `current` has the grouping set of the group.
#[allow(clippy::too_many_arguments)]
fn rows_of_group<'a>(
    eval: &mut Eval<'a>,
    query: &'a Query,
    aggs: &[Agg<'_>],
    group: &[usize],
    tuples: &'a [Vec<usize>],
    none: &'a [usize],
    current: Group,
    levels: &[Vec<&Expr>],
    windowed: bool,
    kept: &mut Vec<(&'a [usize], Group)>,
    rows: &mut Vec<Vec<Value>>,
) -> Result<()> {
    eval.group = current;
    {
        let mut states = aggs.iter().map(|a| a.start(eval)).collect::<Result<Vec<_>>>()?;
        // The rows of an aggregate with `DISTINCT` or `ORDER BY`, which it reads after the sort.
        let mut sorted: Vec<Vec<Vec<Value>>> = vec![Vec::new(); aggs.len()];
        for &i in group {
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
        // The direct arguments of an ordered-set aggregate read the first row of the group.
        eval.tuple = group.first().map_or(none, |&i| &tuples[i]);
        let mut values = Vec::with_capacity(aggs.len());
        for ((agg, mut state), mut inputs) in aggs.iter().zip(states).zip(sorted) {
            if let Method::OrderedSet(ordered) = agg.method {
                let direct =
                    agg.agg.direct.iter().map(|e| eval.eval(e)).collect::<Result<Vec<_>>>()?;
                let value = agg.ordered_set(ordered, &direct, inputs, eval)?;
                values.push((agg.key, value));
                continue;
            }
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
            values.push((agg.key, agg.finish(state, eval)?));
        }
        eval.group.aggs = values;
        if let Some(having) = &query.having
            && eval.eval(having)? != Value::Bool(true)
        {
            return Ok(());
        }
        if !windowed {
            eval.project(&query.targets, levels, rows, usize::MAX)?;
        } else {
            kept.push((eval.tuple, std::mem::take(&mut eval.group)));
        }
    }
    Ok(())
}
