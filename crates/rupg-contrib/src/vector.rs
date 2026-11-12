//! The extension `vector` of pgvector: the types `vector` of `real`, `halfvec` of `half` and `sparsevec`, their functions and the distances of `bit`. A port of `src/vector.c`, `src/halfvec.c`, `src/sparsevec.c` and `src/bitvec.c` of pgvector 0.8.7.
//!
//! A value of `vector` is the data of its varlena: the dimensions as an `int16`, an `int16` that is 0, and an `f32` for each dimension, in the byte order of the machine as pgvector keeps it. A value of `halfvec` is the same with the bits of a `half` for each dimension. The kernels compute as the C code does, with the sums in `float` where pgvector has them in `float`.

mod half;
mod sparse;

use rupg_common::{Error, Result, SqlState};
use rupg_func::{Call, Kernel};
use rupg_types::{Array, Value};

use crate::support::index_support;

/// `VECTOR_MAX_DIM` and `HALFVEC_MAX_DIM`: the most dimensions of a vector.
const VECTOR_MAX_DIM: i32 = 16000;

/// The C symbols of `vector` and their kernels.
pub(crate) static SYMBOLS: &[(&str, Kernel)] = &[
    ("array_to_halfvec", array_to_vector::<Half>),
    ("array_to_sparsevec", sparse::array_to_sparsevec),
    ("array_to_vector", array_to_vector::<Float>),
    ("binary_quantize", binary_quantize::<Float>),
    ("cosine_distance", cosine_distance::<Float>),
    ("halfvec", vector::<Half>),
    ("halfvec_accum", vector_accum::<Half>),
    ("halfvec_add", vector_add::<Half>),
    ("halfvec_avg", vector_avg::<Half>),
    ("halfvec_binary_quantize", binary_quantize::<Half>),
    ("halfvec_cmp", vector_cmp::<Half>),
    ("halfvec_concat", vector_concat::<Half>),
    ("halfvec_cosine_distance", cosine_distance::<Half>),
    ("halfvec_eq", vector_eq::<Half>),
    ("halfvec_ge", vector_ge::<Half>),
    ("halfvec_gt", vector_gt::<Half>),
    ("halfvec_in", vector_in::<Half>),
    ("halfvec_inner_product", inner_product::<Half>),
    ("halfvec_l1_distance", l1_distance::<Half>),
    ("halfvec_l2_distance", l2_distance::<Half>),
    ("halfvec_l2_norm", vector_norm::<Half>),
    ("halfvec_l2_normalize", l2_normalize::<Half>),
    ("halfvec_l2_squared_distance", vector_l2_squared_distance::<Half>),
    ("halfvec_le", vector_le::<Half>),
    ("halfvec_lt", vector_lt::<Half>),
    ("halfvec_mul", vector_mul::<Half>),
    ("halfvec_ne", vector_ne::<Half>),
    ("halfvec_negative_inner_product", vector_negative_inner_product::<Half>),
    ("halfvec_out", vector_out::<Half>),
    ("halfvec_recv", vector_recv::<Half>),
    ("halfvec_send", vector_send::<Half>),
    ("halfvec_spherical_distance", vector_spherical_distance::<Half>),
    ("halfvec_sub", vector_sub::<Half>),
    ("halfvec_subvector", subvector::<Half>),
    ("halfvec_to_float4", vector_to_float4::<Half>),
    ("halfvec_to_sparsevec", sparse::dense_to_sparsevec::<Half>),
    ("halfvec_to_vector", convert::<Half, Float>),
    ("halfvec_typmod_in", vector_typmod_in::<Half>),
    ("halfvec_vector_dims", vector_dims::<Half>),
    ("hamming_distance", hamming_distance),
    ("hnsw_bit_support", index_support),
    ("hnsw_halfvec_support", index_support),
    ("hnsw_sparsevec_support", index_support),
    ("hnswhandler", index_support),
    ("inner_product", inner_product::<Float>),
    ("ivfflat_bit_support", index_support),
    ("ivfflat_halfvec_support", index_support),
    ("ivfflathandler", index_support),
    ("jaccard_distance", jaccard_distance),
    ("l1_distance", l1_distance::<Float>),
    ("l2_distance", l2_distance::<Float>),
    ("l2_normalize", l2_normalize::<Float>),
    ("sparsevec", sparse::sparsevec),
    ("sparsevec_cmp", sparse::sparsevec_cmp),
    ("sparsevec_cosine_distance", sparse::sparsevec_cosine_distance),
    ("sparsevec_eq", sparse::sparsevec_eq),
    ("sparsevec_ge", sparse::sparsevec_ge),
    ("sparsevec_gt", sparse::sparsevec_gt),
    ("sparsevec_in", sparse::sparsevec_in),
    ("sparsevec_inner_product", sparse::sparsevec_inner_product),
    ("sparsevec_l1_distance", sparse::sparsevec_l1_distance),
    ("sparsevec_l2_distance", sparse::sparsevec_l2_distance),
    ("sparsevec_l2_norm", sparse::sparsevec_l2_norm),
    ("sparsevec_l2_normalize", sparse::sparsevec_l2_normalize),
    ("sparsevec_l2_squared_distance", sparse::sparsevec_l2_squared_distance),
    ("sparsevec_le", sparse::sparsevec_le),
    ("sparsevec_lt", sparse::sparsevec_lt),
    ("sparsevec_ne", sparse::sparsevec_ne),
    ("sparsevec_negative_inner_product", sparse::sparsevec_negative_inner_product),
    ("sparsevec_out", sparse::sparsevec_out),
    ("sparsevec_recv", sparse::sparsevec_recv),
    ("sparsevec_send", sparse::sparsevec_send),
    ("sparsevec_to_halfvec", sparse::sparsevec_to_dense::<Half>),
    ("sparsevec_to_vector", sparse::sparsevec_to_dense::<Float>),
    ("sparsevec_typmod_in", sparse::sparsevec_typmod_in),
    ("subvector", subvector::<Float>),
    ("vector", vector::<Float>),
    ("vector_accum", vector_accum::<Float>),
    ("vector_add", vector_add::<Float>),
    ("vector_avg", vector_avg::<Float>),
    ("vector_cmp", vector_cmp::<Float>),
    ("vector_combine", vector_combine),
    ("vector_concat", vector_concat::<Float>),
    ("vector_dims", vector_dims::<Float>),
    ("vector_eq", vector_eq::<Float>),
    ("vector_ge", vector_ge::<Float>),
    ("vector_gt", vector_gt::<Float>),
    ("vector_in", vector_in::<Float>),
    ("vector_l2_squared_distance", vector_l2_squared_distance::<Float>),
    ("vector_le", vector_le::<Float>),
    ("vector_lt", vector_lt::<Float>),
    ("vector_mul", vector_mul::<Float>),
    ("vector_ne", vector_ne::<Float>),
    ("vector_negative_inner_product", vector_negative_inner_product::<Float>),
    ("vector_norm", vector_norm::<Float>),
    ("vector_out", vector_out::<Float>),
    ("vector_recv", vector_recv::<Float>),
    ("vector_send", vector_send::<Float>),
    ("vector_spherical_distance", vector_spherical_distance::<Float>),
    ("vector_sub", vector_sub::<Float>),
    ("vector_to_float4", vector_to_float4::<Float>),
    ("vector_to_halfvec", convert::<Float, Half>),
    ("vector_to_sparsevec", sparse::dense_to_sparsevec::<Float>),
    ("vector_typmod_in", vector_typmod_in::<Float>),
];

fn wrong_kind() -> Error {
    Error::internal("a function got a value of the wrong kind")
}

/// The kind of the elements of a dense vector: `float` for `vector` and `half` for `halfvec`. A kernel computes with the elements as `float`, as pgvector does with `HalfToFloat4`, and gives an element back to the type with [`Dense::round`].
trait Dense {
    /// The name of the type in the messages.
    const NAME: &'static str;
    /// The bytes of an element.
    const SIZE: usize;
    /// The element nearest to a `float`, with no check: `Float4ToHalfUnchecked` for `halfvec`.
    fn round(v: f32) -> f32;
    /// An element from its bytes, in the byte order of the machine or of the network.
    fn read(bytes: &[u8], network: bool) -> f32;
    /// The bytes of an element.
    fn write(v: f32, out: &mut Vec<u8>, network: bool);
}

/// The elements of `vector`.
struct Float;

impl Dense for Float {
    const NAME: &'static str = "vector";
    const SIZE: usize = 4;

    fn round(v: f32) -> f32 {
        v
    }

    fn read(bytes: &[u8], network: bool) -> f32 {
        let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if network { f32::from_be_bytes(bytes) } else { f32::from_ne_bytes(bytes) }
    }

    fn write(v: f32, out: &mut Vec<u8>, network: bool) {
        out.extend_from_slice(&if network { v.to_be_bytes() } else { v.to_ne_bytes() });
    }
}

/// The elements of `halfvec`.
struct Half;

impl Dense for Half {
    const NAME: &'static str = "halfvec";
    const SIZE: usize = 2;

    fn round(v: f32) -> f32 {
        half::to_f32(half::from_f32(v))
    }

    fn read(bytes: &[u8], network: bool) -> f32 {
        let bytes = [bytes[0], bytes[1]];
        half::to_f32(if network { u16::from_be_bytes(bytes) } else { u16::from_ne_bytes(bytes) })
    }

    fn write(v: f32, out: &mut Vec<u8>, network: bool) {
        let h = half::from_f32(v);
        out.extend_from_slice(&if network { h.to_be_bytes() } else { h.to_ne_bytes() });
    }
}

/// The elements of a value of `vector` or `halfvec`.
fn decode<K: Dense>(value: &Value) -> Result<Vec<f32>> {
    let Value::Bytea(data) = value else { return Err(wrong_kind()) };
    if data.len() < 4 {
        return Err(wrong_kind());
    }
    let dim = i16::from_ne_bytes([data[0], data[1]]);
    let elements = &data[4..];
    if dim < 0 || elements.len() != dim as usize * K::SIZE {
        return Err(wrong_kind());
    }
    Ok(elements.chunks(K::SIZE).map(|c| K::read(c, false)).collect())
}

/// `InitVector` or `InitHalfVector` with the elements, which are values of the kind.
fn encode<K: Dense>(x: &[f32]) -> Value {
    let mut data = Vec::with_capacity(4 + K::SIZE * x.len());
    // The dimensions are at most VECTOR_MAX_DIM, so they fit.
    #[allow(clippy::cast_possible_truncation)]
    data.extend_from_slice(&(x.len() as i16).to_ne_bytes());
    data.extend_from_slice(&0i16.to_ne_bytes());
    for v in x {
        K::write(*v, &mut data, false);
    }
    Value::Bytea(data)
}

fn two<K: Dense>(args: &[Value]) -> Result<(Vec<f32>, Vec<f32>)> {
    let [a, b, ..] = args else { return Err(wrong_kind()) };
    Ok((decode::<K>(a)?, decode::<K>(b)?))
}

fn one<K: Dense>(args: &[Value]) -> Result<Vec<f32>> {
    decode::<K>(args.first().ok_or_else(wrong_kind)?)
}

fn int4_arg(args: &[Value], i: usize) -> Result<i32> {
    match args.get(i) {
        Some(Value::Int4(v)) => Ok(*v),
        _ => Err(wrong_kind()),
    }
}

fn data_exception(message: impl Into<String>) -> Error {
    Error::new(SqlState::DATA_EXCEPTION, message)
}

/// `CheckDims`.
fn check_dims<K: Dense>(a: &[f32], b: &[f32]) -> Result<()> {
    if a.len() == b.len() {
        Ok(())
    } else {
        Err(data_exception(format!("different {} dimensions {} and {}", K::NAME, a.len(), b.len())))
    }
}

/// `CheckExpectedDim`.
fn check_expected_dim(typmod: i32, dim: usize) -> Result<()> {
    if typmod != -1 && usize::try_from(typmod).ok() != Some(dim) {
        return Err(data_exception(format!("expected {typmod} dimensions, not {dim}")));
    }
    Ok(())
}

/// `CheckDim`.
fn check_dim<K: Dense>(dim: i64) -> Result<()> {
    if dim < 1 {
        return Err(data_exception(format!("{} must have at least 1 dimension", K::NAME)));
    }
    if dim > i64::from(VECTOR_MAX_DIM) {
        return Err(Error::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("{} cannot have more than {VECTOR_MAX_DIM} dimensions", K::NAME),
        ));
    }
    Ok(())
}

/// `CheckElement`.
fn check_element<K: Dense>(value: f32) -> Result<()> {
    if value.is_nan() {
        return Err(data_exception(format!("NaN not allowed in {}", K::NAME)));
    }
    if value.is_infinite() {
        return Err(data_exception(format!("infinite value not allowed in {}", K::NAME)));
    }
    Ok(())
}

/// The shortest text of a `float`, as `float_to_shortest_decimal_buf` gives it.
fn shortest(v: f32) -> String {
    let mut out = Vec::new();
    rupg_types::float4_out(v, 1, &mut out);
    String::from_utf8(out).unwrap_or_default()
}

/// The element of the kind for a `float`, with an error for a finite value out of its range: `Float4ToHalf` for `halfvec`.
fn checked<K: Dense>(v: f32) -> Result<f32> {
    let r = K::round(v);
    if r.is_infinite() && !v.is_infinite() {
        return Err(Error::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("\"{}\" is out of range for type {}", shortest(v), K::NAME),
        ));
    }
    Ok(r)
}

/// `float_overflow_error`.
fn overflow() -> Error {
    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow")
}

/// `float_underflow_error`.
fn underflow() -> Error {
    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: underflow")
}

/// `scanner_isspace`, which `vector_isspace` is from PostgreSQL 17.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'\x0b')
}

/// `strtof` on the start of `s`: the value, the number of bytes that it read, and whether it sets `ERANGE`, for a value that is infinite or 0 though its text is not. `None` when `s` does not start with a number.
fn strtof(s: &[u8]) -> Option<(f32, usize, bool)> {
    let mut i = 0;
    if matches!(s.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let rest = &s[i..];
    let starts =
        |word: &[u8]| rest.len() >= word.len() && rest[..word.len()].eq_ignore_ascii_case(word);
    if starts(b"infinity") || starts(b"inf") || starts(b"nan") {
        let len = if starts(b"infinity") { 8 } else { 3 };
        let negative = s[0] == b'-';
        let value = if starts(b"nan") {
            f32::NAN
        } else if negative {
            f32::NEG_INFINITY
        } else {
            f32::INFINITY
        };
        return Some((value, i + len, false));
    }
    let digits = |from: usize| s[from..].iter().take_while(|c| c.is_ascii_digit()).count();
    let whole = digits(i);
    i += whole;
    let mut fraction = 0;
    if s.get(i) == Some(&b'.') {
        fraction = digits(i + 1);
        if whole + fraction > 0 {
            i += 1 + fraction;
        }
    }
    if whole + fraction == 0 {
        return None;
    }
    let nonzero = s[..i].iter().any(|c| matches!(c, b'1'..=b'9'));
    if matches!(s.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(s.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        let exponent = digits(j);
        if exponent > 0 {
            i = j + exponent;
        }
    }
    let text = std::str::from_utf8(&s[..i]).ok()?;
    let value: f32 = text.parse().ok()?;
    Some((value, i, value.is_infinite() || (value == 0.0 && nonzero)))
}

fn vector_in<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Some(Value::Text(lit)), Ok(typmod)) = (args.first(), int4_arg(args, 2)) else {
        return Err(wrong_kind());
    };
    let syntax = || {
        Error::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type {}: \"{lit}\"", K::NAME),
        )
    };
    let s = lit.as_bytes();
    let mut pt = 0;
    let skip = |pt: &mut usize| {
        while s.get(*pt).copied().is_some_and(is_space) {
            *pt += 1;
        }
    };
    skip(&mut pt);
    if s.get(pt) != Some(&b'[') {
        return Err(syntax().with_detail("Vector contents must start with \"[\"."));
    }
    pt += 1;
    skip(&mut pt);
    if s.get(pt) == Some(&b']') {
        return Err(data_exception(format!("{} must have at least 1 dimension", K::NAME)));
    }
    let mut x = Vec::new();
    loop {
        if x.len() == VECTOR_MAX_DIM as usize {
            return Err(Error::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("{} cannot have more than {VECTOR_MAX_DIM} dimensions", K::NAME),
            ));
        }
        skip(&mut pt);
        if pt == s.len() {
            return Err(syntax());
        }
        let Some((val, len, range)) = strtof(&s[pt..]) else { return Err(syntax()) };
        let v = K::round(val);
        if (range && val.is_infinite()) || (v.is_infinite() && !val.is_infinite()) {
            return Err(Error::new(
                SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                format!("\"{}\" is out of range for type {}", &lit[pt..pt + len], K::NAME),
            ));
        }
        check_element::<K>(v)?;
        x.push(v);
        pt += len;
        skip(&mut pt);
        match s.get(pt) {
            Some(b',') => pt += 1,
            Some(b']') => {
                pt += 1;
                break;
            }
            _ => return Err(syntax()),
        }
    }
    skip(&mut pt);
    if pt != s.len() {
        return Err(syntax().with_detail("Junk after closing right brace."));
    }
    check_dim::<K>(x.len() as i64)?;
    check_expected_dim(typmod, x.len())?;
    Ok(encode::<K>(&x))
}

fn vector_out<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = one::<K>(args)?;
    let mut out = Vec::with_capacity(x.len() * 8 + 2);
    out.push(b'[');
    for (i, v) in x.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        rupg_types::float4_out(*v, 1, &mut out);
    }
    out.push(b']');
    Ok(Value::Text(String::from_utf8(out).map_err(|_| wrong_kind())?))
}

/// `ArrayGetIntegerTypmods` of a type modifier of one integer, with the checks of a `typmod_in` of pgvector for the type `name` of at most `max` dimensions.
fn typmod(args: &[Value], name: &str, max: i32) -> Result<Value> {
    let Some(Value::Array(array)) = args.first() else { return Err(wrong_kind()) };
    let invalid = |message: String| Error::new(SqlState::INVALID_PARAMETER_VALUE, message);
    if array.values.len() != 1 {
        return Err(invalid("invalid type modifier".into()));
    }
    let n = match &array.values[0] {
        Some(Value::Text(text)) => text.trim().parse::<i32>().map_err(|_| {
            Error::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!("invalid input syntax for type integer: \"{text}\""),
            )
        })?,
        Some(Value::Int4(v)) => *v,
        _ => {
            return Err(Error::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "typmod array must not contain nulls",
            ));
        }
    };
    if n < 1 {
        return Err(invalid(format!("dimensions for type {name} must be at least 1")));
    }
    if n > max {
        return Err(invalid(format!("dimensions for type {name} cannot exceed {max}")));
    }
    Ok(Value::Int4(n))
}

fn vector_typmod_in<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    typmod(args, K::NAME, VECTOR_MAX_DIM)
}

fn short_message() -> Error {
    Error::new(SqlState::PROTOCOL_VIOLATION, "insufficient data left in message")
}

/// The error of a receive function that did not read all of its message.
fn long_message() -> Error {
    Error::new(
        SqlState::INVALID_BINARY_REPRESENTATION,
        "incorrect binary data format in function argument",
    )
}

fn vector_recv<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Some(Value::Bytea(buf)), Ok(typmod)) = (args.first(), int4_arg(args, 2)) else {
        return Err(wrong_kind());
    };
    if buf.len() < 4 {
        return Err(short_message());
    }
    let dim = i16::from_be_bytes([buf[0], buf[1]]);
    let unused = i16::from_be_bytes([buf[2], buf[3]]);
    check_dim::<K>(i64::from(dim))?;
    let dim = dim as usize;
    check_expected_dim(typmod, dim)?;
    if unused != 0 {
        return Err(data_exception(format!("expected unused to be 0, not {unused}")));
    }
    let data = &buf[4..];
    if data.len() < K::SIZE * dim {
        return Err(short_message());
    }
    let mut x = Vec::with_capacity(dim);
    for c in data[..K::SIZE * dim].chunks(K::SIZE) {
        let v = K::read(c, true);
        check_element::<K>(v)?;
        x.push(v);
    }
    if data.len() > K::SIZE * dim {
        return Err(long_message());
    }
    Ok(encode::<K>(&x))
}

fn vector_send<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = one::<K>(args)?;
    let mut out = Vec::with_capacity(4 + K::SIZE * x.len());
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(x.len() as i16).to_be_bytes());
    out.extend_from_slice(&0i16.to_be_bytes());
    for v in &x {
        K::write(*v, &mut out, true);
    }
    Ok(Value::Bytea(out))
}

/// `vector(vector, integer, boolean)` or `halfvec(halfvec, integer, boolean)`: the cast to a type with a number of dimensions.
fn vector<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = one::<K>(args)?;
    check_expected_dim(int4_arg(args, 1)?, x.len())?;
    Ok(args[0].clone())
}

/// The cast between `vector` and `halfvec`: `vector_to_halfvec` and `halfvec_to_vector`.
fn convert<From: Dense, To: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = one::<From>(args)?;
    check_dim::<To>(x.len() as i64)?;
    check_expected_dim(int4_arg(args, 1)?, x.len())?;
    let x = x.into_iter().map(checked::<To>).collect::<Result<Vec<_>>>()?;
    Ok(encode::<To>(&x))
}

/// `float4` of a `numeric`, as `numeric_float4` gives it.
fn numeric_to_f32(v: &rupg_types::Numeric) -> Result<f32> {
    let mut text = Vec::new();
    rupg_types::numeric_out(v, &mut text);
    let text = String::from_utf8(text).map_err(|_| wrong_kind())?;
    rupg_types::float4_in(&text).map_err(Error::from)
}

/// The elements of an array argument of a cast to a vector type as `float`, with its checks: `array must be 1-D`, no null, and the element types `integer`, `double precision`, `real` and `numeric`.
fn array_elements(array: &Array<Value>) -> Result<Vec<f32>> {
    if array.dims.len() > 1 {
        return Err(data_exception("array must be 1-D"));
    }
    if array.values.iter().any(Option::is_none) {
        return Err(Error::new(SqlState::NULL_VALUE_NOT_ALLOWED, "array must not contain nulls"));
    }
    let mut x = Vec::with_capacity(array.values.len());
    for value in array.values.iter().flatten() {
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let v = match value {
            Value::Int4(v) => *v as f32,
            Value::Float8(v) => *v as f32,
            Value::Float4(v) => *v,
            Value::Numeric(v) => numeric_to_f32(v)?,
            _ => return Err(data_exception("unsupported array type")),
        };
        x.push(v);
    }
    Ok(x)
}

fn array_to_vector<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Array(array)) = args.first() else { return Err(wrong_kind()) };
    let typmod = int4_arg(args, 1)?;
    if array.dims.len() <= 1 && array.values.iter().all(Option::is_some) {
        check_dim::<K>(array.values.len() as i64)?;
        check_expected_dim(typmod, array.values.len())?;
    }
    let x = array_elements(array)?.into_iter().map(checked::<K>).collect::<Result<Vec<_>>>()?;
    for v in &x {
        check_element::<K>(*v)?;
    }
    Ok(encode::<K>(&x))
}

fn vector_to_float4<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = one::<K>(args)?;
    let values = x.into_iter().map(|v| Some(Value::Float4(v))).collect();
    Ok(Value::Array(Box::new(Array::one(values))))
}

/// `VectorL2SquaredDistance`.
fn l2_squared(a: &[f32], b: &[f32]) -> f32 {
    let mut distance = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        let diff = x - y;
        distance += diff * diff;
    }
    distance
}

/// `VectorInnerProduct`.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut distance = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        distance += x * y;
    }
    distance
}

/// The two vectors of a function of two vectors of the same dimensions.
fn same<K: Dense>(args: &[Value]) -> Result<(Vec<f32>, Vec<f32>)> {
    let (a, b) = two::<K>(args)?;
    check_dims::<K>(&a, &b)?;
    Ok((a, b))
}

fn l2_distance<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same::<K>(args)?;
    Ok(Value::Float8(f64::from(l2_squared(&a, &b)).sqrt()))
}

fn vector_l2_squared_distance<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same::<K>(args)?;
    Ok(Value::Float8(f64::from(l2_squared(&a, &b))))
}

fn inner_product<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same::<K>(args)?;
    Ok(Value::Float8(f64::from(dot(&a, &b))))
}

fn vector_negative_inner_product<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same::<K>(args)?;
    Ok(Value::Float8(f64::from(-dot(&a, &b))))
}

/// `1 - similarity`, with the similarity kept in [-1, 1]. A NaN stays NaN, as the comparisons of the C code keep it.
fn cosine(similarity: f64) -> Value {
    Value::Float8(1.0 - similarity.clamp(-1.0, 1.0))
}

fn cosine_distance<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same::<K>(args)?;
    let (mut similarity, mut norma, mut normb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(&b) {
        similarity += x * y;
        norma += x * x;
        normb += y * y;
    }
    Ok(cosine(f64::from(similarity) / (f64::from(norma) * f64::from(normb)).sqrt()))
}

fn vector_spherical_distance<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same::<K>(args)?;
    let distance = f64::from(dot(&a, &b)).clamp(-1.0, 1.0);
    Ok(Value::Float8(distance.acos() / std::f64::consts::PI))
}

fn l1_distance<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same::<K>(args)?;
    let mut distance = 0.0f32;
    for (x, y) in a.iter().zip(&b) {
        distance += (x - y).abs();
    }
    Ok(Value::Float8(f64::from(distance)))
}

fn vector_dims<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = one::<K>(args)?;
    Ok(Value::Int4(i32::try_from(x.len()).map_err(|_| wrong_kind())?))
}

/// The norm of the elements, with the sum in `double`.
fn norm(x: &[f32]) -> f64 {
    x.iter().fold(0.0f64, |n, v| n + f64::from(*v) * f64::from(*v)).sqrt()
}

fn vector_norm<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float8(norm(&one::<K>(args)?)))
}

fn l2_normalize<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let a = one::<K>(args)?;
    let norm = norm(&a);
    let mut r = vec![0.0f32; a.len()];
    if norm > 0.0 {
        for (r, x) in r.iter_mut().zip(&a) {
            #[allow(clippy::cast_possible_truncation)]
            {
                *r = K::round((f64::from(*x) / norm) as f32);
            }
        }
        if r.iter().any(|v| v.is_infinite()) {
            return Err(overflow());
        }
    }
    Ok(encode::<K>(&r))
}

/// The elements of `a` and `b` combined by `f` and given back to the kind, with an error for an infinite element.
fn elementwise<K: Dense>(args: &[Value], f: impl Fn(f32, f32) -> f32) -> Result<Value> {
    let (a, b) = same::<K>(args)?;
    let r: Vec<f32> = a.iter().zip(&b).map(|(x, y)| K::round(f(*x, *y))).collect();
    if r.iter().any(|v| v.is_infinite()) {
        return Err(overflow());
    }
    Ok(encode::<K>(&r))
}

fn vector_add<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    elementwise::<K>(args, |x, y| x + y)
}

fn vector_sub<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    elementwise::<K>(args, |x, y| x - y)
}

fn vector_mul<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same::<K>(args)?;
    let r: Vec<f32> = a.iter().zip(&b).map(|(x, y)| K::round(x * y)).collect();
    for i in 0..r.len() {
        if r[i].is_infinite() {
            return Err(overflow());
        }
        if r[i] == 0.0 && !(a[i] == 0.0 || b[i] == 0.0) {
            return Err(underflow());
        }
    }
    Ok(encode::<K>(&r))
}

fn vector_concat<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (mut a, b) = two::<K>(args)?;
    check_dim::<K>((a.len() + b.len()) as i64)?;
    a.extend_from_slice(&b);
    Ok(encode::<K>(&a))
}

/// `binary_quantize`: a `bit` with a 1 for each element that is more than 0.
fn binary_quantize<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = one::<K>(args)?;
    Ok(Value::Text(x.iter().map(|v| if *v > 0.0 { '1' } else { '0' }).collect()))
}

fn subvector<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let a = one::<K>(args)?;
    let mut start = i64::from(int4_arg(args, 1)?);
    let count = i64::from(int4_arg(args, 2)?);
    let dim = a.len() as i64;
    let empty = || data_exception(format!("{} must have at least 1 dimension", K::NAME));
    if count < 1 {
        return Err(empty());
    }
    let end = if start > dim - count { dim + 1 } else { start + count };
    if start < 1 {
        start = 1;
    } else if start > dim {
        return Err(empty());
    }
    let len = end - start;
    check_dim::<K>(len)?;
    let from = (start - 1) as usize;
    Ok(encode::<K>(&a[from..from + len as usize]))
}

/// The sign of an ordering, as an `int4` of a comparison function.
fn sign(ordering: std::cmp::Ordering) -> i32 {
    match ordering {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// `vector_cmp_internal`.
fn cmp<K: Dense>(args: &[Value]) -> Result<i32> {
    let (a, b) = two::<K>(args)?;
    for (x, y) in a.iter().zip(&b) {
        if x < y {
            return Ok(-1);
        }
        if x > y {
            return Ok(1);
        }
    }
    Ok(sign(a.len().cmp(&b.len())))
}

fn vector_lt<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp::<K>(args)? < 0))
}

fn vector_le<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp::<K>(args)? <= 0))
}

fn vector_eq<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp::<K>(args)? == 0))
}

fn vector_ne<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp::<K>(args)? != 0))
}

fn vector_ge<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp::<K>(args)? >= 0))
}

fn vector_gt<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp::<K>(args)? > 0))
}

fn vector_cmp<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int4(cmp::<K>(args)?))
}

/// `CheckStateArray`: the values of the state of `avg`, the count and the sum of each dimension.
fn state(value: &Value, caller: &str) -> Result<Vec<f64>> {
    let bad = || Error::internal(format!("{caller}: expected state array"));
    let Value::Array(array) = value else { return Err(bad()) };
    if array.dims.len() != 1 || array.values.is_empty() {
        return Err(bad());
    }
    array
        .values
        .iter()
        .map(|v| match v {
            Some(Value::Float8(v)) => Ok(*v),
            _ => Err(bad()),
        })
        .collect()
}

fn state_value(values: Vec<f64>) -> Value {
    Value::Array(Box::new(Array::one(values.into_iter().map(|v| Some(Value::Float8(v))).collect())))
}

fn vector_accum<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [state_arg, new, ..] = args else { return Err(wrong_kind()) };
    let values = state(state_arg, &format!("{}_accum", K::NAME))?;
    let x = decode::<K>(new)?;
    let dim = values.len() - 1;
    let newarr = dim == 0;
    if !newarr {
        check_expected_dim(i32::try_from(dim).map_err(|_| wrong_kind())?, x.len())?;
    }
    let mut out = Vec::with_capacity(x.len() + 1);
    out.push(values[0] + 1.0);
    if newarr {
        out.extend(x.iter().map(|v| f64::from(*v)));
    } else {
        for (s, v) in values[1..].iter().zip(&x) {
            let v = s + f64::from(*v);
            if v.is_infinite() {
                return Err(overflow());
            }
            out.push(v);
        }
    }
    Ok(state_value(out))
}

fn vector_combine(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b, ..] = args else { return Err(wrong_kind()) };
    let a = state(a, "vector_combine")?;
    let b = state(b, "vector_combine")?;
    let (dim1, dim2) = (a.len() - 1, b.len() - 1);
    let mut out = vec![a[0] + b[0]];
    if dim1 == 0 && dim2 == 0 {
    } else if dim1 == 0 {
        check_dim::<Float>(dim2 as i64)?;
        out.extend_from_slice(&b[1..]);
    } else if dim2 == 0 {
        check_dim::<Float>(dim1 as i64)?;
        out.extend_from_slice(&a[1..]);
    } else {
        check_dim::<Float>(dim1 as i64)?;
        check_expected_dim(i32::try_from(dim1).map_err(|_| wrong_kind())?, dim2)?;
        for (x, y) in a[1..].iter().zip(&b[1..]) {
            let v = x + y;
            if v.is_infinite() {
                return Err(overflow());
            }
            out.push(v);
        }
    }
    Ok(state_value(out))
}

fn vector_avg<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let values = state(args.first().ok_or_else(wrong_kind)?, &format!("{}_avg", K::NAME))?;
    let n = values[0];
    if n == 0.0 {
        return Ok(Value::Null);
    }
    check_dim::<K>((values.len() - 1) as i64)?;
    let mut x = Vec::with_capacity(values.len() - 1);
    for v in &values[1..] {
        #[allow(clippy::cast_possible_truncation)]
        let v = checked::<K>((v / n) as f32)?;
        check_element::<K>(v)?;
        x.push(v);
    }
    Ok(encode::<K>(&x))
}

/// The bits of a value of `bit`, which is the text of its digits.
fn bits(value: &Value) -> Result<&[u8]> {
    match value {
        Value::Text(text) => Ok(text.as_bytes()),
        _ => Err(wrong_kind()),
    }
}

/// The two values of a distance of `bit`, which have the same length.
fn two_bits(args: &[Value]) -> Result<(&[u8], &[u8])> {
    let [a, b, ..] = args else { return Err(wrong_kind()) };
    let (a, b) = (bits(a)?, bits(b)?);
    if a.len() != b.len() {
        return Err(data_exception(format!("different bit lengths {} and {}", a.len(), b.len())));
    }
    Ok((a, b))
}

/// `hamming_distance`: the number of the bits that differ.
fn hamming_distance(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = two_bits(args)?;
    let distance = a.iter().zip(b).filter(|(x, y)| x != y).count();
    #[allow(clippy::cast_precision_loss)]
    Ok(Value::Float8(distance as f64))
}

/// `jaccard_distance`: 1 less the bits set in both over the bits set in either.
fn jaccard_distance(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = two_bits(args)?;
    let ones = |x: &[u8]| x.iter().filter(|c| **c == b'1').count();
    let ab = a.iter().zip(b).filter(|(x, y)| **x == b'1' && **y == b'1').count();
    let (aa, bb) = (ones(a), ones(b));
    if ab == 0 {
        return Ok(Value::Float8(1.0));
    }
    #[allow(clippy::cast_precision_loss)]
    Ok(Value::Float8(1.0 - ab as f64 / (aa + bb - ab) as f64))
}

#[cfg(test)]
mod tests {
    use rupg_types::oid;

    use super::*;
    use crate::pg_trgm::test_session::TestSession;

    /// `vector_send` gives the form that `vector_recv` reads, and `vector_recv` checks the dimensions and the elements.
    #[test]
    fn binary() {
        let session = TestSession::default();
        let value = encode::<Float>(&[1.5, -2.0, 0.25]);
        let Value::Bytea(sent) = session
            .call(vector_send::<Float>, &[0], oid::BYTEA, std::slice::from_ref(&value))
            .unwrap()
        else {
            panic!("not bytea")
        };
        assert_eq!(&sent[..4], &[0, 3, 0, 0]);
        assert_eq!(&sent[4..8], &1.5f32.to_be_bytes());
        let recv = |bytes: Vec<u8>, typmod: i32| {
            session.call(
                vector_recv::<Float>,
                &[oid::INTERNAL, oid::OID, oid::INT4],
                0,
                &[Value::Bytea(bytes), Value::Oid(0), Value::Int4(typmod)],
            )
        };
        assert_eq!(recv(sent.clone(), -1).unwrap(), value);
        assert_eq!(recv(sent.clone(), 2).unwrap_err().message(), "expected 2 dimensions, not 3");
        let mut nan = sent;
        nan[4..8].copy_from_slice(&f32::NAN.to_be_bytes());
        assert_eq!(recv(nan, -1).unwrap_err().message(), "NaN not allowed in vector");
        assert_eq!(
            recv(vec![0, 1, 0, 1, 0, 0, 0, 0], -1).unwrap_err().message(),
            "expected unused to be 0, not 1"
        );
    }

    #[test]
    fn strtof_prefix() {
        assert_eq!(strtof(b"1.5,2"), Some((1.5, 3, false)));
        assert_eq!(strtof(b"-1e3]"), Some((-1000.0, 4, false)));
        assert_eq!(strtof(b"1e]"), Some((1.0, 1, false)));
        assert_eq!(strtof(b".5"), Some((0.5, 2, false)));
        assert_eq!(strtof(b"4e38"), Some((f32::INFINITY, 4, true)));
        assert_eq!(strtof(b"1e-46"), Some((0.0, 5, true)));
        assert_eq!(strtof(b"0e-46"), Some((0.0, 5, false)));
        assert_eq!(strtof(b"."), None);
        assert_eq!(strtof(b"abc"), None);
        assert_eq!(strtof(b"-Infinity").map(|(v, n, _)| (v, n)), Some((f32::NEG_INFINITY, 9)));
    }
}
