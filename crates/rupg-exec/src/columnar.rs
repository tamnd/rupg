//! The vector scan: a query of one table whose columns the session keeps in segments reads the encoded vectors, and not the rows.
//!
//! The scan tests the conditions of `WHERE` on each vector of 1024 rows with a selection vector, and it makes the groups and the aggregates with the codes and the integers of the vectors. The targets, `HAVING` and `ORDER BY` then run with [`Eval`] on one row for each group, or on each row that passes, so that they give the values of the row engine. The groups come in the order that the row engine gives them, which is the order of the hash table of `HashAggregate`. A query that the scan cannot run gives `None`, and the row engine runs it.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Arc;

use rupg_analyze::{Aggref, BoolOp, Expr, ExprKind, FromItem, Query, Var};
use rupg_column::dict::Dictionary;
use rupg_column::segment::{Kind, Nulls, ROWS, Store};
use rupg_column::stored::{self, Stored, VECTOR};
use rupg_common::{Error, Result, SqlState};
use rupg_func::Session;
use rupg_pgcatalog::builtin;
use rupg_types::{Numeric, Value, oid};

use crate::agg;
use crate::{Cache, Eval, Group, hashed, scan, sort};

/// The most entries of the memo of an expression of one column of integers, so that a column of many values does not fill the memory.
const MEMO: usize = 1 << 20;

/// A hasher for keys that are words, as `FxHasher` of rustc.
#[derive(Clone, Copy, Default)]
struct Fx(u64);

impl Fx {
    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for word in words {
            self.add(u64::from_le_bytes(*word));
        }
        for &byte in rest {
            self.add(u64::from(byte));
        }
    }

    fn write_u64(&mut self, word: u64) {
        self.add(word);
    }

    fn write_usize(&mut self, word: usize) {
        self.add(word as u64);
    }
}

type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;
type FxSet<K> = HashSet<K, BuildHasherDefault<Fx>>;

/// The physical type that a column of the type `ty` has in a segment.
fn kind_of(ty: u32) -> Option<Kind> {
    Some(match ty {
        oid::BOOL => Kind::Bool,
        oid::INT2 => Kind::Int2,
        oid::INT4 => Kind::Int4,
        oid::INT8 => Kind::Int8,
        oid::FLOAT4 => Kind::Float4,
        oid::FLOAT8 => Kind::Float8,
        oid::DATE => Kind::Date,
        oid::TIME => Kind::Time,
        oid::TIMESTAMP => Kind::Timestamp,
        oid::TIMESTAMPTZ => Kind::TimestampTz,
        oid::TEXT | oid::VARCHAR | oid::BPCHAR => Kind::Text,
        oid::BYTEA => Kind::Bytea,
        _ => return None,
    })
}

/// True for a collation whose equality is the equality of the bytes: the default collation, `C` and `POSIX`.
fn plain(collation: u32) -> bool {
    matches!(collation, 0 | 100 | 950 | 951)
}

/// The integer of a value of an integer type, a date or a time.
fn int_of(value: &Value) -> Option<i64> {
    Some(match value {
        Value::Bool(v) => i64::from(*v),
        Value::Int2(v) => i64::from(*v),
        Value::Int4(v) => i64::from(*v),
        Value::Int8(v) => *v,
        Value::Date(v) => i64::from(*v),
        Value::Time(v) | Value::Timestamp(v) | Value::TimestampTz(v) => *v,
        _ => return None,
    })
}

/// The word of a float as a key: `-0` is `0`, and each NaN is the same NaN, as `float8eq` finds them equal.
fn float_word(v: f64) -> u64 {
    if v == 0.0 {
        0
    } else if v.is_nan() {
        f64::NAN.to_bits()
    } else {
        v.to_bits()
    }
}

/// The strings of a column of codes.
enum Dict {
    /// The dictionary of a column of the table.
    Stored(Arc<Stored>),
    /// The dictionary of the text values that an expression gives.
    Own(Dictionary),
}

impl Dict {
    fn value(&self, code: u32) -> Value {
        match self {
            Dict::Stored(stored) => {
                let dict = stored.dict.as_ref().map(|d| d.get(code)).unwrap_or_default();
                stored.kind.bytes_value(dict)
            }
            Dict::Own(dict) => Kind::Text.bytes_value(dict.get(code)),
        }
    }

    fn find(&self, bytes: &[u8]) -> u32 {
        match self {
            Dict::Stored(stored) => {
                stored.dict.as_ref().map_or(rupg_column::dict::MISSING, |d| d.find(bytes))
            }
            Dict::Own(dict) => dict.find(bytes),
        }
    }
}

/// The values of an expression for the rows of a selection.
enum Data {
    Int(Vec<i64>),
    Float(Vec<f64>),
    /// Codes into a dictionary of [`Exec::dicts`]. A null is the code 0.
    Code(Vec<u32>, usize),
    Values(Vec<Value>),
    /// The same value for each row.
    Const(Value),
}

/// The values of an expression of the type `ty` for the rows of a selection, with the null rows. A `boolean` has the integers 0 and 1. A `numeric` of [`Data::Int`] is an integer.
struct Col {
    data: Data,
    nulls: Option<Vec<bool>>,
    ty: u32,
}

impl Col {
    fn constant(value: Value, ty: u32) -> Col {
        Col { data: Data::Const(value), nulls: None, ty }
    }

    fn bools(values: &[Option<bool>]) -> Col {
        let nulls = values
            .iter()
            .any(Option::is_none)
            .then(|| values.iter().map(Option::is_none).collect());
        let data = Data::Int(values.iter().map(|v| i64::from(v.unwrap_or(false))).collect());
        Col { data, nulls, ty: oid::BOOL }
    }

    #[inline]
    fn null(&self, i: usize) -> bool {
        match &self.data {
            Data::Const(v) => v.is_null(),
            Data::Values(v) => v[i].is_null(),
            _ => self.nulls.as_ref().is_some_and(|n| n[i]),
        }
    }

    /// The value of a row as a condition: `None` for null.
    #[inline]
    fn truth(&self, i: usize) -> Option<bool> {
        if self.null(i) {
            return None;
        }
        match &self.data {
            Data::Int(v) => Some(v[i] != 0),
            Data::Const(Value::Bool(b)) => Some(*b),
            Data::Values(v) => match v[i] {
                Value::Bool(b) => Some(b),
                _ => None,
            },
            _ => None,
        }
    }

    /// The integer of a row of [`Data::Int`] or of a constant integer, or `None` for null.
    #[inline]
    fn int(&self, i: usize) -> Option<i64> {
        if self.null(i) {
            return None;
        }
        match &self.data {
            Data::Int(v) => Some(v[i]),
            Data::Const(v) => int_of(v),
            _ => None,
        }
    }

    fn is_int(&self) -> bool {
        match &self.data {
            Data::Int(_) => true,
            Data::Const(v) => v.is_null() || int_of(v).is_some(),
            _ => false,
        }
    }

    /// The value of a row.
    fn value(&self, i: usize, dicts: &[Dict]) -> Value {
        if self.null(i) {
            return Value::Null;
        }
        match &self.data {
            Data::Int(v) => int_value(self.ty, v[i]),
            Data::Float(v) => {
                if self.ty == oid::FLOAT4 {
                    Value::Float4(v[i] as f32)
                } else {
                    Value::Float8(v[i])
                }
            }
            Data::Code(v, d) => dicts[*d].value(v[i]),
            Data::Values(v) => v[i].clone(),
            Data::Const(v) => v.clone(),
        }
    }

    /// The word of a row as a key of a group, or `None` for a value that has no word.
    #[inline]
    fn word(&self, i: usize) -> Option<u64> {
        Some(match &self.data {
            Data::Int(v) => v[i] as u64,
            Data::Float(v) => float_word(v[i]),
            Data::Code(v, _) => u64::from(v[i]),
            Data::Const(_) => 0,
            Data::Values(_) => return None,
        })
    }
}

/// The value of the integer `v` of a column of the type `ty`.
fn int_value(ty: u32, v: i64) -> Value {
    match ty {
        oid::NUMERIC => Value::Numeric(Numeric::from_integer(i128::from(v))),
        _ => kind_of(ty).map_or(Value::Int8(v), |k| k.int_value(v)),
    }
}

/// The values of a column in one vector.
struct Decoded {
    data: DecodedData,
    nulls: Option<Vec<bool>>,
}

enum DecodedData {
    Int(Vec<i64>),
    Float(Vec<f64>),
    Code(Vec<u32>),
}

/// One vector of the table.
struct Batch {
    segment: usize,
    vector: usize,
    rows: usize,
    /// The decoded vector of each column that the scan read, by its place in [`Exec::stored`].
    decoded: Vec<Option<Decoded>>,
}

/// A memo of the values of an expression of one column, by the value of the column.
enum Memo {
    /// By the code, for a column of codes.
    Code(Vec<Option<(Value, u32)>>),
    /// By the word of the integer or the float.
    Word(FxMap<u64, (Value, u32)>),
}

/// An expression that the scan runs with [`Eval`]: a copy whose columns are parameters after the parameters of the query.
struct Generic {
    template: Expr,
    /// The columns of the expression, in the order of their parameters.
    vars: Vec<Var>,
    memo: Option<Memo>,
    /// The memo of a null value of the column.
    null: Option<(Value, u32)>,
    /// The dictionary of the text values of the expression.
    dict: Option<usize>,
}

/// The state of the scan of one query.
struct Exec<'q> {
    params: &'q [Value],
    session: &'q dyn Session,
    cache: &'q Cache,
    /// The columns that the scan reads.
    stored: Vec<Arc<Stored>>,
    /// The place of each column of the table in `stored`, or `usize::MAX`.
    places: Vec<usize>,
    /// The type of each column of the table.
    types: Vec<u32>,
    /// The dictionary of each column of codes in `stored`, by its place.
    dict_of: Vec<usize>,
    dicts: Vec<Dict>,
    generic: FxMap<*const Expr, Generic>,
    /// The parameters of the query, then the values of the columns of a [`Generic`].
    slots: Vec<Value>,
    empty: scan::Tables,
}

/// The source name of the function of a call.
fn src(oid: u32) -> &'static str {
    builtin::proc_by_oid(oid).map_or("", |p| p.src)
}

/// The comparison of a function of integers, dates, times and booleans.
fn int_compare(src: &str) -> Option<fn(i64, i64) -> bool> {
    let (head, op) = src.split_at_checked(src.len().checked_sub(2)?)?;
    let head = head.strip_suffix('_').unwrap_or(head);
    if !matches!(
        head,
        "int2"
            | "int4"
            | "int8"
            | "int24"
            | "int42"
            | "int28"
            | "int82"
            | "int48"
            | "int84"
            | "date"
            | "timestamp"
            | "timestamptz"
            | "time"
            | "bool"
    ) {
        return None;
    }
    Some(match op {
        "eq" => |a, b| a == b,
        "ne" => |a, b| a != b,
        "lt" => |a, b| a < b,
        "le" => |a, b| a <= b,
        "gt" => |a, b| a > b,
        "ge" => |a, b| a >= b,
        _ => return None,
    })
}

/// The arithmetic of a function of integers.
fn int_arith(src: &str) -> Option<fn(i64, i64) -> Option<i64>> {
    let head = ["int2", "int4", "int8", "int24", "int42", "int28", "int82", "int48", "int84"]
        .into_iter()
        .filter(|h| src.starts_with(h))
        .max_by_key(|h| h.len())?;
    Some(match &src[head.len()..] {
        "pl" => i64::checked_add,
        "mi" => i64::checked_sub,
        "mul" => i64::checked_mul,
        _ => return None,
    })
}

/// The range of the integer type `ty`.
fn int_range(ty: u32) -> Option<(i64, i64)> {
    Some(match ty {
        oid::INT2 => (i64::from(i16::MIN), i64::from(i16::MAX)),
        oid::INT4 => (i64::from(i32::MIN), i64::from(i32::MAX)),
        oid::INT8 => (i64::MIN, i64::MAX),
        _ => return None,
    })
}

/// The microseconds of the unit of `date_trunc` and `extract` that the scan computes for a `timestamp`.
fn unit(field: &Expr) -> Option<i64> {
    let ExprKind::Const(Value::Text(name)) = &field.kind else { return None };
    Some(match name.as_str() {
        "second" => 1_000_000,
        "minute" => 60_000_000,
        "hour" => 3_600_000_000,
        "day" => 86_400_000_000,
        _ => return None,
    })
}

impl<'q> Exec<'q> {
    /// The vector of the column `column` for the selection `sel`.
    fn var(&mut self, batch: &mut Batch, column: usize, ty: u32, sel: &[u32]) -> Result<Col> {
        let place = self.places[column];
        self.decode(batch, place)?;
        let Some(decoded) = &batch.decoded[place] else {
            return Err(Error::internal("a vector is not decoded"));
        };
        let nulls = decoded.nulls.as_ref().map(|n| sel.iter().map(|&i| n[i as usize]).collect());
        let data = match &decoded.data {
            DecodedData::Int(v) => Data::Int(sel.iter().map(|&i| v[i as usize]).collect()),
            DecodedData::Float(v) => Data::Float(sel.iter().map(|&i| v[i as usize]).collect()),
            DecodedData::Code(v) => {
                Data::Code(sel.iter().map(|&i| v[i as usize]).collect(), self.dict_of[place])
            }
        };
        Ok(Col { data, nulls, ty })
    }

    fn decode(&self, batch: &mut Batch, place: usize) -> Result<()> {
        if batch.decoded[place].is_some() {
            return Ok(());
        }
        let column = &self.stored[place].segments[batch.segment];
        let vector = &column.vectors[batch.vector];
        let rows = vector.rows as usize;
        let nulls = match &vector.nulls {
            Nulls::None => None,
            Nulls::All => Some(vec![true; rows]),
            some => Some((0..rows).map(|i| some.is_null(i)).collect()),
        };
        let all = vector.nulls == Nulls::All;
        let data = match column.kind.store() {
            Store::Int => {
                let mut out = Vec::new();
                if all {
                    out.resize(rows, 0);
                } else {
                    column.decode_ints(batch.vector, &mut out)?;
                }
                DecodedData::Int(out)
            }
            Store::Float => {
                let mut out = Vec::new();
                if all {
                    out.resize(rows, 0.0);
                } else {
                    column.decode_floats(batch.vector, &mut out)?;
                }
                DecodedData::Float(out)
            }
            Store::Code => {
                let mut out = Vec::new();
                if all {
                    out.resize(rows, 0);
                } else {
                    column.decode_codes(batch.vector, &mut out)?;
                }
                if let Some(nulls) = &nulls {
                    for (code, null) in out.iter_mut().zip(nulls) {
                        if *null {
                            *code = 0;
                        }
                    }
                }
                DecodedData::Code(out)
            }
        };
        if data_len(&data) != rows {
            return Err(Error::corrupted("a vector has another number of values than its rows"));
        }
        batch.decoded[place] = Some(Decoded { data, nulls });
        Ok(())
    }

    /// The values of `expr` for the rows `sel` of the batch.
    fn eval(&mut self, expr: &Expr, batch: &mut Batch, sel: &[u32]) -> Result<Col> {
        match &expr.kind {
            ExprKind::Var(var) => {
                return self.var(batch, usize::try_from(var.attnum - 1).unwrap_or(0), expr.ty, sel);
            }
            ExprKind::Const(value) => return Ok(Col::constant(value.clone(), expr.ty)),
            ExprKind::Param(n) => {
                let value = self.params.get(n.wrapping_sub(1)).cloned().unwrap_or(Value::Null);
                return Ok(Col::constant(value, expr.ty));
            }
            ExprKind::Relabel(arg, _)
                if kind_of(expr.ty).is_some() && kind_of(expr.ty) == kind_of(arg.ty) =>
            {
                let mut col = self.eval(arg, batch, sel)?;
                col.ty = expr.ty;
                return Ok(col);
            }
            ExprKind::Bool(op, args) => return self.boolean(*op, args, batch, sel),
            ExprKind::NullTest(arg, is_null) if kind_of(arg.ty).is_some() => {
                let col = self.eval(arg, batch, sel)?;
                if !matches!(col.data, Data::Values(_)) {
                    let out: Vec<Option<bool>> =
                        (0..sel.len()).map(|i| Some(col.null(i) == *is_null)).collect();
                    return Ok(Col::bools(&out));
                }
            }
            ExprKind::Func(func) if !func.retset => {
                if let Some(col) = self.func(expr, &func.args, src(func.oid), batch, sel)? {
                    return Ok(col);
                }
            }
            _ => {}
        }
        self.generic(expr, batch, sel)
    }

    /// `AND`, `OR` and `NOT` with the logic of three values. Each argument runs only on the rows whose value it can change, as the row engine stops at the first argument that decides the value.
    fn boolean(
        &mut self,
        op: BoolOp,
        args: &[Expr],
        batch: &mut Batch,
        sel: &[u32],
    ) -> Result<Col> {
        if op == BoolOp::Not {
            let col = self.eval(&args[0], batch, sel)?;
            let out: Vec<Option<bool>> = (0..sel.len()).map(|i| col.truth(i).map(|b| !b)).collect();
            return Ok(Col::bools(&out));
        }
        // The value that decides: `false` for `AND`, `true` for `OR`.
        let decides = op == BoolOp::Or;
        let mut out = vec![Some(!decides); sel.len()];
        let mut live: Vec<usize> = (0..sel.len()).collect();
        let mut sub = Vec::new();
        for arg in args {
            if live.is_empty() {
                break;
            }
            sub.clear();
            sub.extend(live.iter().map(|&i| sel[i]));
            let col = self.eval(arg, batch, &sub)?;
            let mut next = Vec::with_capacity(live.len());
            for (k, &i) in live.iter().enumerate() {
                match col.truth(k) {
                    Some(b) if b == decides => out[i] = Some(decides),
                    None => {
                        out[i] = None;
                        next.push(i);
                    }
                    Some(_) => next.push(i),
                }
            }
            live = next;
        }
        Ok(Col::bools(&out))
    }

    /// A call of a function that the scan computes on the vectors, or `None`.
    fn func(
        &mut self,
        expr: &Expr,
        args: &[Expr],
        src: &str,
        batch: &mut Batch,
        sel: &[u32],
    ) -> Result<Option<Col>> {
        if let (Some(compare), [a, b]) = (int_compare(src), args) {
            let (a, b) = (self.eval(a, batch, sel)?, self.eval(b, batch, sel)?);
            if a.is_int() && b.is_int() {
                let out: Vec<Option<bool>> =
                    (0..sel.len()).map(|i| Some(compare(a.int(i)?, b.int(i)?))).collect();
                return Ok(Some(Col::bools(&out)));
            }
            return Ok(None);
        }
        if let ("texteq" | "textne", [a, b]) = (src, args) {
            let equal = src == "texteq";
            let (a, b) = (self.eval(a, batch, sel)?, self.eval(b, batch, sel)?);
            let (code, other) = match (&a.data, &b.data) {
                (Data::Code(..), _) => (&a, &b),
                (_, Data::Code(..)) => (&b, &a),
                _ => return Ok(None),
            };
            let Data::Code(codes, d) = &code.data else { return Ok(None) };
            let out: Vec<Option<bool>> = match &other.data {
                Data::Const(Value::Null) => vec![None; sel.len()],
                Data::Const(Value::Text(text)) => {
                    let want = self.dicts[*d].find(text.as_bytes());
                    codes.iter().map(|&c| (c != 0).then_some((c == want) == equal)).collect()
                }
                Data::Code(others, e) if e == d => codes
                    .iter()
                    .zip(others)
                    .map(|(&c, &o)| (c != 0 && o != 0).then_some((c == o) == equal))
                    .collect(),
                _ => return Ok(None),
            };
            return Ok(Some(Col::bools(&out)));
        }
        if let (Some(op), Some((low, high)), [a, b]) = (int_arith(src), int_range(expr.ty), args) {
            let (a, b) = (self.eval(a, batch, sel)?, self.eval(b, batch, sel)?);
            if !a.is_int() || !b.is_int() {
                return Ok(None);
            }
            let mut values = Vec::with_capacity(sel.len());
            let mut nulls = Vec::with_capacity(sel.len());
            for i in 0..sel.len() {
                match (a.int(i), b.int(i)) {
                    (Some(x), Some(y)) => match op(x, y).filter(|v| (low..=high).contains(v)) {
                        Some(v) => {
                            values.push(v);
                            nulls.push(false);
                        }
                        // The row engine gives the error of the overflow.
                        None => return Ok(None),
                    },
                    _ => {
                        values.push(0);
                        nulls.push(true);
                    }
                }
            }
            let nulls = nulls.contains(&true).then_some(nulls);
            return Ok(Some(Col { data: Data::Int(values), nulls, ty: expr.ty }));
        }
        if let ("timestamp_trunc" | "extract_timestamp", [field, arg]) = (src, args)
            && arg.ty == oid::TIMESTAMP
            && let Some(unit) = unit(field)
        {
            let trunc = src == "timestamp_trunc";
            if !trunc && !(expr.ty == oid::NUMERIC && matches!(unit, 60_000_000 | 3_600_000_000)) {
                return Ok(None);
            }
            let col = self.eval(arg, batch, sel)?;
            if !col.is_int() {
                return Ok(None);
            }
            let mut values = Vec::with_capacity(sel.len());
            let mut nulls = Vec::with_capacity(sel.len());
            for i in 0..sel.len() {
                let value = col.int(i).and_then(|v| {
                    let infinite = v == i64::MIN || v == i64::MAX;
                    if trunc {
                        Some(if infinite { v } else { v - v.rem_euclid(unit) })
                    } else if infinite {
                        None
                    } else {
                        let span = if unit == 60_000_000 { 60 } else { 24 };
                        Some(v.div_euclid(unit).rem_euclid(span))
                    }
                });
                values.push(value.unwrap_or(0));
                nulls.push(value.is_none());
            }
            let nulls = nulls.contains(&true).then_some(nulls);
            return Ok(Some(Col { data: Data::Int(values), nulls, ty: expr.ty }));
        }
        Ok(None)
    }

    /// The values of `expr` from [`Eval`], with its columns as parameters. An expression of one column keeps its value for each value of the column.
    fn generic(&mut self, expr: &Expr, batch: &mut Batch, sel: &[u32]) -> Result<Col> {
        let key = std::ptr::from_ref(expr);
        let mut generic = match self.generic.remove(&key) {
            Some(generic) => generic,
            None => self.template(expr),
        };
        let result = self.run_generic(&mut generic, expr, batch, sel);
        self.generic.insert(key, generic);
        result
    }

    fn template(&mut self, expr: &Expr) -> Generic {
        let mut vars: Vec<Var> = Vec::new();
        let mut template = expr.clone();
        let base = self.params.len();
        template.each_mut(0, &mut |e, _| {
            if let ExprKind::Var(var) = e.kind {
                let at = match vars.iter().position(|v| v.attnum == var.attnum) {
                    Some(at) => at,
                    None => {
                        vars.push(var);
                        vars.len() - 1
                    }
                };
                e.kind = ExprKind::Param(base + at + 1);
                return true;
            }
            false
        });
        let dict = (kind_of(expr.ty) == Some(Kind::Text)).then(|| {
            self.dicts.push(Dict::Own(Dictionary::new()));
            self.dicts.len() - 1
        });
        Generic { template, vars, memo: None, null: None, dict }
    }

    /// The value of the template of `generic` with the slots of its columns.
    fn call(&mut self, generic: &Generic) -> Result<(Value, u32)> {
        let value = {
            let mut eval = Eval::new(&self.slots, self.session, &self.empty, self.cache, &[]);
            eval.eval(&generic.template)?
        };
        let code = match (&value, generic.dict) {
            (Value::Text(text), Some(d)) => match &mut self.dicts[d] {
                Dict::Own(dict) => dict.intern(text.as_bytes()),
                Dict::Stored(_) => 0,
            },
            _ => 0,
        };
        Ok((value, code))
    }

    fn run_generic(
        &mut self,
        generic: &mut Generic,
        expr: &Expr,
        batch: &mut Batch,
        sel: &[u32],
    ) -> Result<Col> {
        let base = self.params.len();
        self.slots.truncate(base);
        self.slots.resize(base + generic.vars.len(), Value::Null);
        let mut inputs = Vec::with_capacity(generic.vars.len());
        for var in &generic.vars {
            let column = usize::try_from(var.attnum - 1).unwrap_or(0);
            inputs.push(self.var(batch, column, self.types[column], sel)?);
        }
        let mut out: Vec<(Value, u32)> = Vec::with_capacity(sel.len());
        match inputs.as_slice() {
            [] => {
                if generic.null.is_none() {
                    generic.null = Some(self.call(generic)?);
                }
                let (value, _) = generic.null.clone().unwrap_or((Value::Null, 0));
                return Ok(Col::constant(value, expr.ty));
            }
            [input] => {
                for i in 0..sel.len() {
                    if input.null(i) {
                        if generic.null.is_none() {
                            self.slots[base] = Value::Null;
                            generic.null = Some(self.call(generic)?);
                        }
                        out.push(generic.null.clone().unwrap_or((Value::Null, 0)));
                        continue;
                    }
                    let found = match (&generic.memo, &input.data) {
                        (Some(Memo::Code(memo)), Data::Code(codes, _)) => {
                            memo.get(codes[i] as usize).and_then(Clone::clone)
                        }
                        (Some(Memo::Word(memo)), _) => {
                            input.word(i).and_then(|w| memo.get(&w).cloned())
                        }
                        _ => None,
                    };
                    if let Some(found) = found {
                        out.push(found);
                        continue;
                    }
                    self.slots[base] = input.value(i, &self.dicts);
                    let result = self.call(generic)?;
                    match (&mut generic.memo, &input.data) {
                        (memo @ None, Data::Code(..)) => *memo = Some(Memo::Code(Vec::new())),
                        (memo @ None, Data::Int(_) | Data::Float(_)) => {
                            *memo = Some(Memo::Word(FxMap::default()));
                        }
                        _ => {}
                    }
                    match (&mut generic.memo, &input.data) {
                        (Some(Memo::Code(memo)), Data::Code(codes, _)) => {
                            let at = codes[i] as usize;
                            if memo.len() <= at {
                                memo.resize(at + 1, None);
                            }
                            memo[at] = Some(result.clone());
                        }
                        (Some(Memo::Word(memo)), _) if memo.len() < MEMO => {
                            if let Some(w) = input.word(i) {
                                memo.insert(w, result.clone());
                            }
                        }
                        _ => {}
                    }
                    out.push(result);
                }
            }
            _ => {
                for i in 0..sel.len() {
                    for (k, input) in inputs.iter().enumerate() {
                        self.slots[base + k] = input.value(i, &self.dicts);
                    }
                    out.push(self.call(generic)?);
                }
            }
        }
        Ok(self.collect(expr.ty, generic.dict, out))
    }

    /// A column of the values of a [`Generic`].
    fn collect(&self, ty: u32, dict: Option<usize>, out: Vec<(Value, u32)>) -> Col {
        let nulls: Vec<bool> = out.iter().map(|(v, _)| v.is_null()).collect();
        let any = nulls.contains(&true);
        if let Some(d) = dict
            && out.iter().all(|(v, c)| v.is_null() || *c != 0)
        {
            let codes = out.iter().map(|(_, c)| *c).collect();
            return Col { data: Data::Code(codes, d), nulls: any.then_some(nulls), ty };
        }
        match kind_of(ty).map(Kind::store) {
            Some(Store::Int) if ty != oid::NUMERIC => {
                let ints: Option<Vec<i64>> = out
                    .iter()
                    .map(|(v, _)| if v.is_null() { Some(0) } else { int_of(v) })
                    .collect();
                if let Some(ints) = ints {
                    return Col { data: Data::Int(ints), nulls: any.then_some(nulls), ty };
                }
            }
            Some(Store::Float) => {
                let floats: Option<Vec<f64>> = out
                    .iter()
                    .map(|(v, _)| match v {
                        Value::Null => Some(0.0),
                        Value::Float4(f) => Some(f64::from(*f)),
                        Value::Float8(f) => Some(*f),
                        _ => None,
                    })
                    .collect();
                if let Some(floats) = floats {
                    return Col { data: Data::Float(floats), nulls: any.then_some(nulls), ty };
                }
            }
            _ => {}
        }
        Col { data: Data::Values(out.into_iter().map(|(v, _)| v).collect()), nulls: None, ty }
    }
}

fn data_len(data: &DecodedData) -> usize {
    match data {
        DecodedData::Int(v) => v.len(),
        DecodedData::Float(v) => v.len(),
        DecodedData::Code(v) => v.len(),
    }
}

/// An expression that the scan cannot run: a subquery, a column of another query or a system column, a call that is volatile, or an aggregate that the scan does not make.
fn unsupported(expr: &Expr) -> bool {
    crate::volatile(expr)
        || expr
            .find(0, &mut |e, _| {
                let bad = match &e.kind {
                    ExprKind::SubLink(_)
                    | ExprKind::SubColumn(_)
                    | ExprKind::Window(_)
                    | ExprKind::Grouping(_)
                    | ExprKind::PlaceHolder(_)
                    | ExprKind::CurrentOf(_)
                    | ExprKind::JsonTable(_) => true,
                    ExprKind::Var(var) => var.levels_up != 0 || var.attnum <= 0,
                    ExprKind::Collate(_, collation) => !plain(*collation),
                    ExprKind::Agg(agg) => {
                        agg.levels_up != 0
                            || agg.kind != b'n'
                            || !agg.order.is_empty()
                            || agg.filter.is_some()
                            || (!agg.distinct.is_empty() && !distinct_count(agg))
                    }
                    _ => false,
                };
                bad.then_some(())
            })
            .is_some()
}

/// True for `count(DISTINCT x)` of a built-in `count`.
fn distinct_count(agg: &Aggref) -> bool {
    agg.oid < rupg_catalog::FIRST_NORMAL_OID
        && agg.nargs == 1
        && agg.args.len() == 1
        && builtin::proc_by_oid(agg.oid).is_some_and(|p| p.name == "count")
        && agg.distinct.iter().all(|d| plain(d.collation))
}

/// The columns that `expr` reads outside the arguments of its aggregate calls.
fn outside_aggs(expr: &Expr, out: &mut [bool]) {
    match &expr.kind {
        ExprKind::Agg(_) => {}
        ExprKind::Var(var) => {
            if let Some(slot) = usize::try_from(var.attnum - 1).ok().and_then(|i| out.get_mut(i)) {
                *slot = true;
            }
        }
        _ => {
            for child in expr.children() {
                outside_aggs(child, out);
            }
        }
    }
}

/// How the scan makes an aggregate.
enum Method<'q> {
    /// `count(*)`.
    Count,
    /// `count(x)`.
    CountArg,
    /// `count(DISTINCT x)`.
    Distinct,
    /// `sum` of `smallint` or `integer`, which gives `bigint`.
    SumInt,
    /// `sum` of `bigint` and `avg` of integers, which give `numeric`.
    Int128 {
        avg: bool,
    },
    Min,
    Max,
    /// The method of the row engine, with the values of the arguments.
    Row(agg::Agg<'q>),
}

/// The states of an aggregate of each group.
enum States {
    Count(Vec<i64>),
    Sum(Vec<(i64, i128)>),
    Extreme(Vec<Option<i64>>),
    Distinct(Vec<FxSet<u64>>),
    Row(Vec<agg::State>),
}

struct Call<'q> {
    agg: &'q Aggref,
    key: *const Aggref,
    method: Method<'q>,
    states: States,
}

/// The method of the scan for an aggregate call.
fn method<'q>(agg: &'q Aggref, expr: &'q Expr) -> Result<Option<Method<'q>>> {
    let name = if agg.oid < rupg_catalog::FIRST_NORMAL_OID {
        builtin::proc_by_oid(agg.oid).map_or("", |p| p.name)
    } else {
        ""
    };
    let arg = agg.args.first().filter(|_| agg.nargs == 1 && agg.args.len() == 1);
    let arg_ty = arg.map_or(0, |a| a.ty);
    let int = matches!(kind_of(arg_ty).map(Kind::store), Some(Store::Int)) && arg_ty != oid::BOOL;
    Ok(Some(match name {
        "count" if agg.star || agg.nargs == 0 => Method::Count,
        "count" if arg.is_some() && !agg.distinct.is_empty() => Method::Distinct,
        "count" if arg.is_some() => Method::CountArg,
        "sum" if matches!(arg_ty, oid::INT2 | oid::INT4) && expr.ty == oid::INT8 => Method::SumInt,
        "sum" if arg_ty == oid::INT8 && expr.ty == oid::NUMERIC => Method::Int128 { avg: false },
        "avg" if matches!(arg_ty, oid::INT2 | oid::INT4 | oid::INT8) && expr.ty == oid::NUMERIC => {
            Method::Int128 { avg: true }
        }
        "min" if int && expr.ty == arg_ty => Method::Min,
        "max" if int && expr.ty == arg_ty => Method::Max,
        _ => {
            if !agg.distinct.is_empty() {
                return Ok(None);
            }
            let Ok(method) = agg::method(agg, expr.ty, expr.location) else { return Ok(None) };
            if matches!(method, agg::Method::OrderedSet(_)) {
                return Ok(None);
            }
            Method::Row(agg::Agg {
                agg,
                key: std::ptr::from_ref(agg),
                ty: expr.ty,
                method,
                types: agg.args[..agg.nargs].iter().map(|a| a.ty).collect(),
            })
        }
    }))
}

/// The rows of a query of one table that the scan can run, or `None` for a query that the row engine runs.
///
/// # Errors
///
/// The error of an expression, as the row engine gives it.
pub(crate) fn run(
    query: &Query,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
    want: Option<usize>,
) -> Result<Option<Vec<Vec<Value>>>> {
    let [relation] = query.relations.as_slice() else { return Ok(None) };
    if !matches!(query.from.as_slice(), [FromItem::Relation(0)])
        || relation.subquery.is_some()
        || relation.function.is_some()
        || relation.values.is_some()
        || relation.cte.is_some()
        || relation.tablesample.is_some()
        || relation.oid < rupg_catalog::FIRST_NORMAL_OID
        || query.set_op.is_some()
        || !query.windows.is_empty()
        || !query.ctes.is_empty()
        || !query.row_marks.is_empty()
        || query.write.is_some()
        || !query.grouping_sets.is_empty()
        || query.with_ties
        || query.has_target_srfs()
    {
        return Ok(None);
    }
    // An index scan gives its rows in the order of the index, which the columns do not keep.
    if session.join_order(query).is_some_and(|planned| planned.iter().any(|s| !s.keys.is_empty())) {
        return Ok(None);
    }
    let Some(catalog) = session.catalog() else { return Ok(None) };
    let Some(table) = catalog.relation(relation.oid) else { return Ok(None) };
    if table.kind != rupg_catalog::RelKind::Table
        || !catalog.all_inheritors(relation.oid).is_empty()
    {
        return Ok(None);
    }
    let exprs = query
        .targets
        .iter()
        .map(|t| &t.expr)
        .chain(&query.filter)
        .chain(&query.having)
        .chain(&query.offset)
        .chain(&query.limit);
    let mut used = vec![false; relation.columns.len()];
    for expr in exprs.clone() {
        if unsupported(expr) {
            return Ok(None);
        }
        expr.find(0, &mut |e, _| {
            if let ExprKind::Var(var) = e.kind
                && let Some(slot) =
                    usize::try_from(var.attnum - 1).ok().and_then(|i| used.get_mut(i))
            {
                *slot = true;
            }
            None::<()>
        });
    }
    for item in &query.group {
        let ty = query.targets[item.target].expr.ty;
        if !plain(item.collation) || ty == oid::BPCHAR {
            return Ok(None);
        }
    }
    let types: Vec<u32> = relation.columns.iter().map(|c| c.ty).collect();
    let mut wanted = Vec::new();
    let mut places = vec![usize::MAX; types.len()];
    for (column, _) in used.iter().enumerate().filter(|(_, u)| **u) {
        let Some(kind) = kind_of(types[column]) else { return Ok(None) };
        if kind == Kind::Text && !table.columns.get(column).is_some_and(|c| plain(c.collation)) {
            return Ok(None);
        }
        places[column] = wanted.len();
        wanted.push((column, kind));
    }
    let Some((rows, stored)) = session.table_columns(relation.oid, &wanted)? else {
        return Ok(None);
    };
    let mut dicts = Vec::new();
    let mut dict_of = vec![usize::MAX; stored.len()];
    for (place, column) in stored.iter().enumerate() {
        if column.kind.store() == Store::Code {
            dict_of[place] = dicts.len();
            dicts.push(Dict::Stored(Arc::clone(column)));
        }
    }
    let mut exec = Exec {
        params,
        session,
        cache,
        stored,
        places,
        types,
        dict_of,
        dicts,
        generic: FxMap::default(),
        slots: params.to_vec(),
        empty: scan::Tables::default(),
    };
    let (offset, count) = {
        let mut eval = Eval::new(params, session, &exec.empty, cache, &[]);
        eval.limits(query)?
    };
    if count == Some(0) {
        return Ok(Some(Vec::new()));
    }
    // `is_degenerate_grouping`: with `HAVING` and no aggregate and no `GROUP BY`, the query does not read the table.
    let calls = agg::calls(query);
    let degenerate =
        query.grouped && query.having.is_some() && query.group.is_empty() && calls.is_empty();
    let open = !degenerate && {
        let mut eval = Eval::new(params, session, &exec.empty, cache, &[]);
        crate::gate(query, &mut eval)?
    };
    let mut conds = Vec::new();
    if let Some(filter) = &query.filter {
        scan::conjuncts(filter, &mut conds);
    }
    let conds: Vec<&Expr> = scan::order_quals(conds, &query.barrier)
        .into_iter()
        .filter(|c| c.first_var().is_some())
        .collect();
    // The columns that the targets and `HAVING` read, which the rows of the result keep.
    let mut kept = vec![false; relation.columns.len()];
    for expr in query.targets.iter().map(|t| &t.expr).chain(&query.having) {
        if query.grouped {
            outside_aggs(expr, &mut kept);
        } else {
            expr.find(0, &mut |e, _| {
                if let ExprKind::Var(var) = e.kind
                    && let Some(slot) =
                        usize::try_from(var.attnum - 1).ok().and_then(|i| kept.get_mut(i))
                {
                    *slot = true;
                }
                None::<()>
            });
        }
    }
    let kept_columns: Vec<usize> = (0..kept.len()).filter(|&c| kept[c]).collect();
    if open {
        session.count_scan(relation.oid);
    }
    let segments = rows.div_ceil(ROWS);
    let mut scan = Scan { exec: &mut exec, conds: &conds, segments, rows, open };
    let (rows, tables) = if query.grouped {
        let Some(out) = grouped(&mut scan, query, &calls, &kept_columns)? else {
            return Ok(None);
        };
        out
    } else {
        let ordered = !query.sort.is_empty() || !query.distinct.is_empty();
        let mut needed = match count {
            Some(count) if !ordered => offset.saturating_add(count),
            _ => usize::MAX,
        };
        if let Some(want) = want
            && !ordered
        {
            needed = needed.min(offset.saturating_add(want));
        }
        let mut found: Vec<Vec<Value>> = Vec::new();
        scan.each(&mut |exec, batch, sel| {
            let sel = &sel[..sel.len().min(needed - found.len())];
            let mut cols = Vec::with_capacity(kept_columns.len());
            for &column in &kept_columns {
                cols.push(exec.var(batch, column, exec.types[column], sel)?);
            }
            for i in 0..sel.len() {
                found.push(cols.iter().map(|c| c.value(i, &exec.dicts)).collect());
            }
            Ok(found.len() < needed)
        })?;
        let tables = scan::kept_tables(query, found, &kept);
        let rows = project(query, params, session, cache, &tables, needed, None, offset, count)?;
        (rows, tables)
    };
    drop(tables);
    let types: Vec<u32> = query.targets.iter().map(|t| t.expr.ty).collect();
    let rows = crate::order(query, rows, &types, offset, count, session)?;
    let width = query.targets.iter().take_while(|t| !t.junk).count();
    Ok(Some(
        rows.into_iter()
            .map(|mut row| {
                row.truncate(width);
                row
            })
            .collect(),
    ))
}

/// The rows of the targets for each row of `tables`, or for each group with the results of its aggregates, which pass `HAVING`.
#[allow(clippy::too_many_arguments)]
fn project(
    query: &Query,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
    tables: &scan::Tables,
    needed: usize,
    groups: Option<Groups>,
    offset: usize,
    count: Option<usize>,
) -> Result<Vec<Vec<Value>>> {
    let levels = crate::set_levels(&query.targets);
    let mut eval = Eval::new(params, session, tables, cache, &[]);
    let mut out = Vec::new();
    match groups {
        Some((order, mut aggs, firsts)) => {
            let tuples: Vec<[usize; 1]> =
                (0..aggs.len()).map(|g| [if g < firsts { g } else { scan::NONE }]).collect();
            for (n, g) in order.into_iter().enumerate() {
                crate::interrupt(session, n)?;
                eval.tuple = &tuples[g];
                eval.group = Group { aggs: std::mem::take(&mut aggs[g]), ..Group::default() };
                if let Some(having) = &query.having
                    && eval.eval(having)? != Value::Bool(true)
                {
                    continue;
                }
                eval.project(&query.targets, &levels, &mut out, usize::MAX)?;
            }
        }
        None => {
            let rows = tables.rows.first().map_or(0, |r| r.len());
            let tuples: Vec<[usize; 1]> = (0..rows).map(|r| [r]).collect();
            let ordered = !query.sort.is_empty() || !query.distinct.is_empty();
            for (n, tuple) in tuples.iter().enumerate() {
                if out.len() >= needed {
                    break;
                }
                crate::interrupt(session, n)?;
                eval.tuple = tuple;
                if let Err(error) = eval.project(&query.targets, &levels, &mut out, needed) {
                    // With no sort, each row went to the client before the target of the next row failed.
                    if !ordered && cache.top.get() == Some(&raw const *query) {
                        let width = query.targets.iter().take_while(|t| !t.junk).count();
                        let sent = out.into_iter().skip(offset).take(count.unwrap_or(usize::MAX));
                        *cache.sent.borrow_mut() = sent
                            .map(|mut row| {
                                row.truncate(width);
                                row
                            })
                            .collect();
                    }
                    return Err(error);
                }
            }
        }
    }
    Ok(out)
}

/// The scan of the vectors of the table with the conditions of `WHERE`.
struct Scan<'e, 'q> {
    exec: &'e mut Exec<'q>,
    conds: &'e [&'e Expr],
    segments: usize,
    rows: usize,
    open: bool,
}

/// The order of the groups, the values of the aggregates of each group, and the number of groups.
type Groups = (Vec<usize>, Vec<Vec<(*const Aggref, Value)>>, usize);

/// A call with the rows of a vector that pass the conditions, which gives false to stop the scan.
type Each<'a> = dyn FnMut(&mut Exec<'_>, &mut Batch, &[u32]) -> Result<bool> + 'a;

impl Scan<'_, '_> {
    /// Calls `each` with the rows of each vector that pass the conditions, until it gives false.
    fn each(&mut self, each: &mut Each<'_>) -> Result<()> {
        if !self.open {
            return Ok(());
        }
        let mut sel: Vec<u32> = Vec::with_capacity(VECTOR);
        let mut next: Vec<u32> = Vec::with_capacity(VECTOR);
        for segment in 0..self.segments {
            let segment_rows = self.rows.saturating_sub(segment * ROWS).min(ROWS);
            for vector in 0..stored::vectors(segment_rows) {
                self.exec.session.interrupted()?;
                let rows = (segment_rows - vector * VECTOR).min(VECTOR);
                let mut batch = Batch {
                    segment,
                    vector,
                    rows,
                    decoded: (0..self.exec.stored.len()).map(|_| None).collect(),
                };
                sel.clear();
                sel.extend(0..batch.rows as u32);
                for cond in self.conds {
                    if sel.is_empty() {
                        break;
                    }
                    let col = self.exec.eval(cond, &mut batch, &sel)?;
                    next.clear();
                    next.extend(
                        sel.iter()
                            .enumerate()
                            .filter(|(k, _)| col.truth(*k) == Some(true))
                            .map(|(_, &i)| i),
                    );
                    std::mem::swap(&mut sel, &mut next);
                }
                if !sel.is_empty() && !each(self.exec, &mut batch, &sel)? {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

impl Exec<'_> {
    /// The value of the column `column` at the row `row` of the batch.
    fn value_at(&mut self, batch: &mut Batch, column: usize, row: u32) -> Result<Value> {
        let col = self.var(batch, column, self.types[column], &[row])?;
        Ok(col.value(0, &self.dicts))
    }
}

/// The rows of a query with groups, and the rows of the first row of each group that the targets read. `None` when a key of `GROUP BY` has no word.
fn grouped(
    scan: &mut Scan<'_, '_>,
    query: &Query,
    calls: &[(&Aggref, &Expr)],
    kept_columns: &[usize],
) -> Result<Option<(Vec<Vec<Value>>, scan::Tables)>> {
    let mut aggs: Vec<Call<'_>> = Vec::with_capacity(calls.len());
    for &(agg, expr) in calls {
        let Some(method) = method(agg, expr)? else { return Ok(None) };
        let states = match &method {
            Method::Count | Method::CountArg => States::Count(Vec::new()),
            Method::Distinct => States::Distinct(Vec::new()),
            Method::SumInt | Method::Int128 { .. } => States::Sum(Vec::new()),
            Method::Min | Method::Max => States::Extreme(Vec::new()),
            Method::Row(_) => States::Row(Vec::new()),
        };
        aggs.push(Call { agg, key: std::ptr::from_ref(agg), method, states });
    }
    let keys: Vec<&Expr> = query.group.iter().map(|g| &query.targets[g.target].expr).collect();
    let mut map: FxMap<Box<[u64]>, u32> = FxMap::default();
    let mut firsts: Vec<Vec<Value>> = Vec::new();
    let mut bail = false;
    let mut word = vec![0_u64; keys.len() + 1];
    let mut ids: Vec<u32> = Vec::with_capacity(VECTOR);
    let params = scan.exec.params;
    let session = scan.exec.session;
    let cache = scan.exec.cache;
    let empty = scan::Tables::default();
    if keys.is_empty() {
        firsts.reserve(1);
        for call in &mut aggs {
            grow(call, 1, params, session, cache, &empty)?;
        }
    }
    let mut groups = usize::from(keys.is_empty());
    let mut first_rows = 0;
    // True when a row of a group came after the first row of the last group.
    let mut trailing = false;
    let mut args: Vec<Value> = Vec::new();
    scan.each(&mut |exec, batch, sel| {
        ids.clear();
        if keys.is_empty() {
            ids.resize(sel.len(), 0);
            if first_rows == 0 {
                let mut row = Vec::with_capacity(kept_columns.len());
                for &column in kept_columns {
                    row.push(exec.value_at(batch, column, sel[0])?);
                }
                firsts.push(row);
                first_rows = 1;
            }
        } else {
            let cols = keys.iter().map(|k| exec.eval(k, batch, sel)).collect::<Result<Vec<_>>>()?;
            for (i, &row_at) in sel.iter().enumerate() {
                let mut mask = 0_u64;
                for (k, col) in cols.iter().enumerate() {
                    if col.null(i) {
                        mask |= 1 << (k % 64);
                        word[k] = 0;
                    } else {
                        let Some(w) = col.word(i) else {
                            bail = true;
                            return Ok(false);
                        };
                        word[k] = w;
                    }
                }
                word[keys.len()] = mask;
                let id = match map.get(&word[..]) {
                    Some(&id) => {
                        trailing = true;
                        id
                    }
                    None => {
                        trailing = false;
                        let id = u32::try_from(groups)
                            .map_err(|_| Error::internal("too many groups"))?;
                        map.insert(word.clone().into_boxed_slice(), id);
                        groups += 1;
                        let mut row = Vec::with_capacity(kept_columns.len());
                        for &column in kept_columns {
                            row.push(exec.value_at(batch, column, row_at)?);
                        }
                        firsts.push(row);
                        for call in &mut aggs {
                            grow(call, groups, params, session, cache, &empty)?;
                        }
                        id
                    }
                };
                ids.push(id);
            }
        }
        for call in &mut aggs {
            let arg = match &call.method {
                Method::Count => None,
                _ => call.agg.args.first(),
            };
            let col = match arg {
                Some(arg) if !matches!(call.method, Method::Row(_)) => {
                    Some(exec.eval(arg, batch, sel)?)
                }
                _ => None,
            };
            match (&call.method, &mut call.states) {
                (Method::Count, States::Count(states)) => {
                    for &id in &ids {
                        states[id as usize] += 1;
                    }
                }
                (Method::CountArg, States::Count(states)) => {
                    let col =
                        col.as_ref().ok_or_else(|| Error::internal("count has no argument"))?;
                    for (i, &id) in ids.iter().enumerate() {
                        if !col.null(i) {
                            states[id as usize] += 1;
                        }
                    }
                }
                (Method::Distinct, States::Distinct(states)) => {
                    let col =
                        col.as_ref().ok_or_else(|| Error::internal("count has no argument"))?;
                    for (i, &id) in ids.iter().enumerate() {
                        if col.null(i) {
                            continue;
                        }
                        let Some(w) = col.word(i) else {
                            bail = true;
                            return Ok(false);
                        };
                        states[id as usize].insert(w);
                    }
                }
                (Method::SumInt | Method::Int128 { .. }, States::Sum(states)) => {
                    let col = col.as_ref().ok_or_else(|| Error::internal("sum has no argument"))?;
                    if !col.is_int() {
                        bail = true;
                        return Ok(false);
                    }
                    for (i, &id) in ids.iter().enumerate() {
                        if let Some(v) = col.int(i) {
                            let state = &mut states[id as usize];
                            state.0 += 1;
                            state.1 += i128::from(v);
                        }
                    }
                }
                (Method::Min | Method::Max, States::Extreme(states)) => {
                    let col = col.as_ref().ok_or_else(|| Error::internal("min has no argument"))?;
                    if !col.is_int() {
                        bail = true;
                        return Ok(false);
                    }
                    let min = matches!(call.method, Method::Min);
                    for (i, &id) in ids.iter().enumerate() {
                        if let Some(v) = col.int(i) {
                            let state = &mut states[id as usize];
                            *state = Some(match *state {
                                None => v,
                                Some(s) if min => s.min(v),
                                Some(s) => s.max(v),
                            });
                        }
                    }
                }
                (Method::Row(agg), States::Row(states)) => {
                    let cols = agg.agg.args[..agg.agg.nargs]
                        .iter()
                        .map(|a| exec.eval(a, batch, sel))
                        .collect::<Result<Vec<_>>>()?;
                    let eval = Eval::new(params, session, &empty, cache, &[]);
                    for (i, &id) in ids.iter().enumerate() {
                        args.clear();
                        args.extend(cols.iter().map(|c| c.value(i, &exec.dicts)));
                        agg.advance(&mut states[id as usize], &args, &eval)?;
                    }
                }
                _ => return Err(Error::internal("an aggregate has another state")),
            }
        }
        Ok(true)
    })?;
    if bail {
        return Ok(None);
    }
    // The results of the aggregates of each group.
    let eval = Eval::new(params, session, &empty, cache, &[]);
    let mut results: Vec<Vec<(*const Aggref, Value)>> =
        (0..groups).map(|_| Vec::with_capacity(aggs.len())).collect();
    for call in aggs {
        let Call { agg, key, method, states } = call;
        match (method, states) {
            (Method::Count | Method::CountArg, States::Count(states)) => {
                for (g, n) in states.into_iter().enumerate() {
                    results[g].push((key, Value::Int8(n)));
                }
            }
            (Method::Distinct, States::Distinct(states)) => {
                for (g, set) in states.into_iter().enumerate() {
                    results[g].push((key, Value::Int8(set.len() as i64)));
                }
            }
            (Method::SumInt, States::Sum(states)) => {
                for (g, (n, sum)) in states.into_iter().enumerate() {
                    let value = if n == 0 {
                        Value::Null
                    } else {
                        Value::Int8(i64::try_from(sum).map_err(|_| {
                            Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range")
                        })?)
                    };
                    results[g].push((key, value));
                }
            }
            (Method::Int128 { avg }, States::Sum(states)) => {
                for (g, (n, sum)) in states.into_iter().enumerate() {
                    let value = match (n, avg) {
                        (0, _) => Value::Null,
                        (_, false) => Value::Numeric(Numeric::from_integer(sum)),
                        (_, true) => Value::Numeric(
                            Numeric::from_integer(sum)
                                .div(&Numeric::from_integer(i128::from(n)))
                                .map_err(rupg_func::type_error)?,
                        ),
                    };
                    results[g].push((key, value));
                }
            }
            (Method::Min | Method::Max, States::Extreme(states)) => {
                let ty = agg.args[0].ty;
                for (g, v) in states.into_iter().enumerate() {
                    results[g].push((key, v.map_or(Value::Null, |v| int_value(ty, v))));
                }
            }
            (Method::Row(agg), States::Row(states)) => {
                for (g, state) in states.into_iter().enumerate() {
                    results[g].push((key, agg.finish(state, &eval)?));
                }
            }
            _ => return Err(Error::internal("an aggregate has another state")),
        }
    }
    drop(eval);
    let first_count = firsts.len();
    let mut kept = vec![false; scan.exec.types.len()];
    for &column in kept_columns {
        kept[column] = true;
    }
    let tables = scan::kept_tables(query, firsts, &kept);
    let order = group_order(query, params, session, cache, &tables, groups, trailing)?;
    let rows = project(
        query,
        params,
        session,
        cache,
        &tables,
        usize::MAX,
        Some((order, results, first_count)),
        0,
        None,
    )?;
    Ok(Some((rows, tables)))
}

/// A new group for an aggregate: the states of `groups` groups.
fn grow(
    call: &mut Call<'_>,
    groups: usize,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
    empty: &scan::Tables,
) -> Result<()> {
    match &mut call.states {
        States::Count(s) => s.resize(groups, 0),
        States::Sum(s) => s.resize(groups, (0, 0)),
        States::Extreme(s) => s.resize(groups, None),
        States::Distinct(s) => s.resize_with(groups, FxSet::default),
        States::Row(s) => {
            if let Method::Row(agg) = &call.method {
                let eval = Eval::new(params, session, empty, cache, &[]);
                while s.len() < groups {
                    s.push(agg.start(&eval)?);
                }
            }
        }
    }
    Ok(())
}

/// The order of the groups, as the row engine gives them: the order of the hash table of the `HashAggregate` of the plan, the order of the keys for a sort, or the order of the first rows.
fn group_order(
    query: &Query,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
    tables: &scan::Tables,
    groups: usize,
    trailing: bool,
) -> Result<Vec<usize>> {
    let first: Vec<usize> = (0..groups).collect();
    if query.group.is_empty() {
        return Ok(first);
    }
    // The values of the keys of each group, from its first row.
    let exprs: Vec<&Expr> = query.group.iter().map(|g| &query.targets[g.target].expr).collect();
    let mut eval = Eval::new(params, session, tables, cache, &[]);
    let tuples: Vec<[usize; 1]> = (0..groups).map(|g| [g]).collect();
    let mut keys = Vec::with_capacity(groups);
    for tuple in &tuples {
        eval.tuple = tuple;
        keys.push(exprs.iter().map(|e| eval.eval(e)).collect::<Result<Vec<_>>>()?);
    }
    let types: Vec<u32> = exprs.iter().map(|e| e.ty).collect();
    if let Some(table) = session.hash_tables(query).and_then(|t| t.group.clone()) {
        let places: Option<Vec<usize>> =
            table.keys.iter().map(|t| query.group.iter().position(|g| g.target == *t)).collect();
        if let Some(places) = places {
            let hash = |g: usize| hashed::hash(places.iter().map(|&p| (types[p], &keys[g][p])));
            if groups == 0 || hash(0).is_ok() {
                let mut hashes = Vec::with_capacity(groups);
                for g in 0..groups {
                    hashes.push(hash(g)?);
                }
                return hashed::order_of(&hashes, table.groups, trailing);
            }
        }
    }
    let list: Vec<rupg_analyze::SortGroup> = query
        .group
        .iter()
        .enumerate()
        .map(|(i, g)| rupg_analyze::SortGroup { target: i, ..g.clone() })
        .collect();
    if list.iter().any(|g| g.sort == 0) {
        return Ok(first);
    }
    let keys_of = sort::Keys::new(&list, &types, session)?;
    let mut order = first;
    sort::qsort(&mut order, &mut |a, b| Ok(keys_of.compare(&keys[*a], &keys[*b])?.then(a.cmp(b))))?;
    Ok(order)
}
