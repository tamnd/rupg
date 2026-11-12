//! The extension `vector` of pgvector: the type `vector` of vectors of `real` and its functions. A port of `src/vector.c` of pgvector 0.8.7.
//!
//! A value of `vector` is the data of its varlena: the dimensions as an `int16`, an `int16` that is 0, and an `f32` for each dimension, in the byte order of the machine as pgvector keeps it. The kernels compute as the C code does, with the sums in `float` where pgvector has them in `float`.

use rupg_common::{Error, Result, SqlState};
use rupg_func::{Call, Kernel};
use rupg_types::{Array, Value};

use crate::support::index_support;

/// `VECTOR_MAX_DIM`: the most dimensions of a vector.
const VECTOR_MAX_DIM: i32 = 16000;

/// The C symbols of `vector` and their kernels.
pub(crate) static SYMBOLS: &[(&str, Kernel)] = &[
    ("array_to_halfvec", not_yet),
    ("array_to_sparsevec", not_yet),
    ("array_to_vector", array_to_vector),
    ("binary_quantize", binary_quantize),
    ("cosine_distance", cosine_distance),
    ("halfvec", not_yet),
    ("halfvec_accum", not_yet),
    ("halfvec_add", not_yet),
    ("halfvec_avg", not_yet),
    ("halfvec_binary_quantize", not_yet),
    ("halfvec_cmp", not_yet),
    ("halfvec_concat", not_yet),
    ("halfvec_cosine_distance", not_yet),
    ("halfvec_eq", not_yet),
    ("halfvec_ge", not_yet),
    ("halfvec_gt", not_yet),
    ("halfvec_in", not_yet),
    ("halfvec_inner_product", not_yet),
    ("halfvec_l1_distance", not_yet),
    ("halfvec_l2_distance", not_yet),
    ("halfvec_l2_norm", not_yet),
    ("halfvec_l2_normalize", not_yet),
    ("halfvec_l2_squared_distance", not_yet),
    ("halfvec_le", not_yet),
    ("halfvec_lt", not_yet),
    ("halfvec_mul", not_yet),
    ("halfvec_ne", not_yet),
    ("halfvec_negative_inner_product", not_yet),
    ("halfvec_out", not_yet),
    ("halfvec_recv", not_yet),
    ("halfvec_send", not_yet),
    ("halfvec_spherical_distance", not_yet),
    ("halfvec_sub", not_yet),
    ("halfvec_subvector", not_yet),
    ("halfvec_to_float4", not_yet),
    ("halfvec_to_sparsevec", not_yet),
    ("halfvec_to_vector", not_yet),
    ("halfvec_typmod_in", not_yet),
    ("halfvec_vector_dims", not_yet),
    ("hamming_distance", not_yet),
    ("hnsw_bit_support", index_support),
    ("hnsw_halfvec_support", index_support),
    ("hnsw_sparsevec_support", index_support),
    ("hnswhandler", index_support),
    ("inner_product", inner_product),
    ("ivfflat_bit_support", index_support),
    ("ivfflat_halfvec_support", index_support),
    ("ivfflathandler", index_support),
    ("jaccard_distance", not_yet),
    ("l1_distance", l1_distance),
    ("l2_distance", l2_distance),
    ("l2_normalize", l2_normalize),
    ("sparsevec", not_yet),
    ("sparsevec_cmp", not_yet),
    ("sparsevec_cosine_distance", not_yet),
    ("sparsevec_eq", not_yet),
    ("sparsevec_ge", not_yet),
    ("sparsevec_gt", not_yet),
    ("sparsevec_in", not_yet),
    ("sparsevec_inner_product", not_yet),
    ("sparsevec_l1_distance", not_yet),
    ("sparsevec_l2_distance", not_yet),
    ("sparsevec_l2_norm", not_yet),
    ("sparsevec_l2_normalize", not_yet),
    ("sparsevec_l2_squared_distance", not_yet),
    ("sparsevec_le", not_yet),
    ("sparsevec_lt", not_yet),
    ("sparsevec_ne", not_yet),
    ("sparsevec_negative_inner_product", not_yet),
    ("sparsevec_out", not_yet),
    ("sparsevec_recv", not_yet),
    ("sparsevec_send", not_yet),
    ("sparsevec_to_halfvec", not_yet),
    ("sparsevec_to_vector", not_yet),
    ("sparsevec_typmod_in", not_yet),
    ("subvector", subvector),
    ("vector", vector),
    ("vector_accum", vector_accum),
    ("vector_add", vector_add),
    ("vector_avg", vector_avg),
    ("vector_cmp", vector_cmp),
    ("vector_combine", vector_combine),
    ("vector_concat", vector_concat),
    ("vector_dims", vector_dims),
    ("vector_eq", vector_eq),
    ("vector_ge", vector_ge),
    ("vector_gt", vector_gt),
    ("vector_in", vector_in),
    ("vector_l2_squared_distance", vector_l2_squared_distance),
    ("vector_le", vector_le),
    ("vector_lt", vector_lt),
    ("vector_mul", vector_mul),
    ("vector_ne", vector_ne),
    ("vector_negative_inner_product", vector_negative_inner_product),
    ("vector_norm", vector_norm),
    ("vector_out", vector_out),
    ("vector_recv", vector_recv),
    ("vector_send", vector_send),
    ("vector_spherical_distance", vector_spherical_distance),
    ("vector_sub", vector_sub),
    ("vector_to_float4", vector_to_float4),
    ("vector_to_halfvec", not_yet),
    ("vector_to_sparsevec", not_yet),
    ("vector_typmod_in", vector_typmod_in),
];

/// The kernel of a function of the types `halfvec`, `sparsevec` and the distances of `bit`, which rupg does not have yet.
fn not_yet(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Err(Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "the types halfvec and sparsevec and the distances of bit are not supported yet",
    ))
}

fn wrong_kind() -> Error {
    Error::internal("a function got a value of the wrong kind")
}

/// The elements of a value of `vector`.
fn decode(value: &Value) -> Result<Vec<f32>> {
    let Value::Bytea(data) = value else { return Err(wrong_kind()) };
    if data.len() < 4 {
        return Err(wrong_kind());
    }
    let dim = i16::from_ne_bytes([data[0], data[1]]);
    let elements = &data[4..];
    if dim < 0 || elements.len() != dim as usize * 4 {
        return Err(wrong_kind());
    }
    Ok(elements.as_chunks::<4>().0.iter().map(|c| f32::from_ne_bytes(*c)).collect())
}

/// `InitVector` with the elements: the value of `vector`.
fn encode(x: &[f32]) -> Value {
    let mut data = Vec::with_capacity(4 + 4 * x.len());
    // The dimensions are at most VECTOR_MAX_DIM, so they fit.
    #[allow(clippy::cast_possible_truncation)]
    data.extend_from_slice(&(x.len() as i16).to_ne_bytes());
    data.extend_from_slice(&0i16.to_ne_bytes());
    for v in x {
        data.extend_from_slice(&v.to_ne_bytes());
    }
    Value::Bytea(data)
}

fn two(args: &[Value]) -> Result<(Vec<f32>, Vec<f32>)> {
    let [a, b, ..] = args else { return Err(wrong_kind()) };
    Ok((decode(a)?, decode(b)?))
}

fn one(args: &[Value]) -> Result<Vec<f32>> {
    decode(args.first().ok_or_else(wrong_kind)?)
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
fn check_dims(a: &[f32], b: &[f32]) -> Result<()> {
    if a.len() == b.len() {
        Ok(())
    } else {
        Err(data_exception(format!("different vector dimensions {} and {}", a.len(), b.len())))
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
fn check_dim(dim: i64) -> Result<()> {
    if dim < 1 {
        return Err(data_exception("vector must have at least 1 dimension"));
    }
    if dim > i64::from(VECTOR_MAX_DIM) {
        return Err(Error::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("vector cannot have more than {VECTOR_MAX_DIM} dimensions"),
        ));
    }
    Ok(())
}

/// `CheckElement`.
fn check_element(value: f32) -> Result<()> {
    if value.is_nan() {
        return Err(data_exception("NaN not allowed in vector"));
    }
    if value.is_infinite() {
        return Err(data_exception("infinite value not allowed in vector"));
    }
    Ok(())
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

/// `strtof` on the start of `s`: the value, the number of bytes that it read, and whether the value is out of the range of `float` (`ERANGE` with an infinite result). `None` when `s` does not start with a number.
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
    Some((value, i, value.is_infinite()))
}

fn vector_in(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Some(Value::Text(lit)), Ok(typmod)) = (args.first(), int4_arg(args, 2)) else {
        return Err(wrong_kind());
    };
    let syntax = || {
        Error::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type vector: \"{lit}\""),
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
        return Err(data_exception("vector must have at least 1 dimension"));
    }
    let mut x = Vec::new();
    loop {
        if x.len() == VECTOR_MAX_DIM as usize {
            return Err(Error::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("vector cannot have more than {VECTOR_MAX_DIM} dimensions"),
            ));
        }
        skip(&mut pt);
        if pt == s.len() {
            return Err(syntax());
        }
        let Some((val, len, range)) = strtof(&s[pt..]) else { return Err(syntax()) };
        if range {
            return Err(Error::new(
                SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                format!("\"{}\" is out of range for type vector", &lit[pt..pt + len]),
            ));
        }
        check_element(val)?;
        x.push(val);
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
    check_dim(x.len() as i64)?;
    check_expected_dim(typmod, x.len())?;
    Ok(encode(&x))
}

fn vector_out(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = one(args)?;
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

fn vector_typmod_in(_: &Call<'_>, args: &[Value]) -> Result<Value> {
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
        return Err(invalid("dimensions for type vector must be at least 1".into()));
    }
    if n > VECTOR_MAX_DIM {
        return Err(invalid(format!("dimensions for type vector cannot exceed {VECTOR_MAX_DIM}")));
    }
    Ok(Value::Int4(n))
}

fn vector_recv(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Some(Value::Bytea(buf)), Ok(typmod)) = (args.first(), int4_arg(args, 2)) else {
        return Err(wrong_kind());
    };
    let short = || Error::new(SqlState::PROTOCOL_VIOLATION, "insufficient data left in message");
    if buf.len() < 4 {
        return Err(short());
    }
    let dim = i16::from_be_bytes([buf[0], buf[1]]);
    let unused = i16::from_be_bytes([buf[2], buf[3]]);
    check_dim(i64::from(dim))?;
    let dim = dim as usize;
    check_expected_dim(typmod, dim)?;
    if unused != 0 {
        return Err(data_exception(format!("expected unused to be 0, not {unused}")));
    }
    let data = &buf[4..];
    if data.len() < 4 * dim {
        return Err(short());
    }
    let mut x = Vec::with_capacity(dim);
    for c in data[..4 * dim].as_chunks::<4>().0 {
        let v = f32::from_be_bytes(*c);
        check_element(v)?;
        x.push(v);
    }
    if data.len() > 4 * dim {
        return Err(Error::new(
            SqlState::INVALID_BINARY_REPRESENTATION,
            "incorrect binary data format in function argument",
        ));
    }
    Ok(encode(&x))
}

fn vector_send(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = one(args)?;
    let mut out = Vec::with_capacity(4 + 4 * x.len());
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(x.len() as i16).to_be_bytes());
    out.extend_from_slice(&0i16.to_be_bytes());
    for v in &x {
        out.extend_from_slice(&v.to_be_bytes());
    }
    Ok(Value::Bytea(out))
}

/// `vector(vector, integer, boolean)`: the cast of a vector to a `vector(n)`.
fn vector(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = one(args)?;
    check_expected_dim(int4_arg(args, 1)?, x.len())?;
    Ok(args[0].clone())
}

/// `float4` of a `numeric`, as `numeric_float4` gives it.
fn numeric_to_f32(v: &rupg_types::Numeric) -> Result<f32> {
    let mut text = Vec::new();
    rupg_types::numeric_out(v, &mut text);
    let text = String::from_utf8(text).map_err(|_| wrong_kind())?;
    rupg_types::float4_in(&text).map_err(Error::from)
}

fn array_to_vector(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Array(array)) = args.first() else { return Err(wrong_kind()) };
    let typmod = int4_arg(args, 1)?;
    if array.dims.len() > 1 {
        return Err(data_exception("array must be 1-D"));
    }
    if array.values.iter().any(Option::is_none) {
        return Err(Error::new(SqlState::NULL_VALUE_NOT_ALLOWED, "array must not contain nulls"));
    }
    check_dim(array.values.len() as i64)?;
    check_expected_dim(typmod, array.values.len())?;
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
    for v in &x {
        check_element(*v)?;
    }
    Ok(encode(&x))
}

fn vector_to_float4(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = one(args)?;
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

fn l2_distance(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = two(args)?;
    check_dims(&a, &b)?;
    Ok(Value::Float8(f64::from(l2_squared(&a, &b)).sqrt()))
}

fn vector_l2_squared_distance(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = two(args)?;
    check_dims(&a, &b)?;
    Ok(Value::Float8(f64::from(l2_squared(&a, &b))))
}

fn inner_product(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = two(args)?;
    check_dims(&a, &b)?;
    Ok(Value::Float8(f64::from(dot(&a, &b))))
}

fn vector_negative_inner_product(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = two(args)?;
    check_dims(&a, &b)?;
    Ok(Value::Float8(f64::from(-dot(&a, &b))))
}

fn cosine_distance(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = two(args)?;
    check_dims(&a, &b)?;
    let (mut similarity, mut norma, mut normb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(&b) {
        similarity += x * y;
        norma += x * x;
        normb += y * y;
    }
    // A NaN stays NaN, as the comparisons of the C code keep it.
    let similarity =
        (f64::from(similarity) / (f64::from(norma) * f64::from(normb)).sqrt()).clamp(-1.0, 1.0);
    Ok(Value::Float8(1.0 - similarity))
}

fn vector_spherical_distance(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = two(args)?;
    check_dims(&a, &b)?;
    let distance = f64::from(dot(&a, &b)).clamp(-1.0, 1.0);
    Ok(Value::Float8(distance.acos() / std::f64::consts::PI))
}

fn l1_distance(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = two(args)?;
    check_dims(&a, &b)?;
    let mut distance = 0.0f32;
    for (x, y) in a.iter().zip(&b) {
        distance += (x - y).abs();
    }
    Ok(Value::Float8(f64::from(distance)))
}

fn vector_dims(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = one(args)?;
    Ok(Value::Int4(i32::try_from(x.len()).map_err(|_| wrong_kind())?))
}

/// The norm of the elements, with the sum in `double`.
fn norm(x: &[f32]) -> f64 {
    x.iter().map(|v| f64::from(*v) * f64::from(*v)).sum::<f64>().sqrt()
}

fn vector_norm(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float8(norm(&one(args)?)))
}

fn l2_normalize(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let a = one(args)?;
    let norm = norm(&a);
    let mut r = vec![0.0f32; a.len()];
    if norm > 0.0 {
        for (r, x) in r.iter_mut().zip(&a) {
            #[allow(clippy::cast_possible_truncation)]
            {
                *r = (f64::from(*x) / norm) as f32;
            }
        }
        if r.iter().any(|v| v.is_infinite()) {
            return Err(overflow());
        }
    }
    Ok(encode(&r))
}

/// The elements of `a` and `b` combined by `f`, with an error for an infinite element.
fn elementwise(
    args: &[Value],
    f: impl Fn(f32, f32) -> f32,
) -> Result<(Vec<f32>, Vec<f32>, Vec<f32>)> {
    let (a, b) = two(args)?;
    check_dims(&a, &b)?;
    let r: Vec<f32> = a.iter().zip(&b).map(|(x, y)| f(*x, *y)).collect();
    if r.iter().any(|v| v.is_infinite()) {
        return Err(overflow());
    }
    Ok((a, b, r))
}

fn vector_add(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (_, _, r) = elementwise(args, |x, y| x + y)?;
    Ok(encode(&r))
}

fn vector_sub(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (_, _, r) = elementwise(args, |x, y| x - y)?;
    Ok(encode(&r))
}

fn vector_mul(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = two(args)?;
    check_dims(&a, &b)?;
    let r: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x * y).collect();
    for i in 0..r.len() {
        if r[i].is_infinite() {
            return Err(overflow());
        }
        if r[i] == 0.0 && !(a[i] == 0.0 || b[i] == 0.0) {
            return Err(underflow());
        }
    }
    Ok(encode(&r))
}

fn vector_concat(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (mut a, b) = two(args)?;
    check_dim((a.len() + b.len()) as i64)?;
    a.extend_from_slice(&b);
    Ok(encode(&a))
}

/// `binary_quantize`: a `bit` with a 1 for each element that is more than 0.
fn binary_quantize(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = one(args)?;
    Ok(Value::Text(x.iter().map(|v| if *v > 0.0 { '1' } else { '0' }).collect()))
}

fn subvector(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let a = one(args)?;
    let mut start = i64::from(int4_arg(args, 1)?);
    let count = i64::from(int4_arg(args, 2)?);
    let dim = a.len() as i64;
    if count < 1 {
        return Err(data_exception("vector must have at least 1 dimension"));
    }
    let end = if start > dim - count { dim + 1 } else { start + count };
    if start < 1 {
        start = 1;
    } else if start > dim {
        return Err(data_exception("vector must have at least 1 dimension"));
    }
    let len = end - start;
    check_dim(len)?;
    let from = (start - 1) as usize;
    Ok(encode(&a[from..from + len as usize]))
}

/// `vector_cmp_internal`.
fn cmp(args: &[Value]) -> Result<i32> {
    let (a, b) = two(args)?;
    for (x, y) in a.iter().zip(&b) {
        if x < y {
            return Ok(-1);
        }
        if x > y {
            return Ok(1);
        }
    }
    Ok(match a.len().cmp(&b.len()) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })
}

fn vector_lt(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp(args)? < 0))
}

fn vector_le(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp(args)? <= 0))
}

fn vector_eq(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp(args)? == 0))
}

fn vector_ne(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp(args)? != 0))
}

fn vector_ge(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp(args)? >= 0))
}

fn vector_gt(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp(args)? > 0))
}

fn vector_cmp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int4(cmp(args)?))
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

fn vector_accum(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [state_arg, new, ..] = args else { return Err(wrong_kind()) };
    let values = state(state_arg, "vector_accum")?;
    let x = decode(new)?;
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
        check_dim(dim2 as i64)?;
        out.extend_from_slice(&b[1..]);
    } else if dim2 == 0 {
        check_dim(dim1 as i64)?;
        out.extend_from_slice(&a[1..]);
    } else {
        check_dim(dim1 as i64)?;
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

fn vector_avg(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let values = state(args.first().ok_or_else(wrong_kind)?, "vector_avg")?;
    let n = values[0];
    if n == 0.0 {
        return Ok(Value::Null);
    }
    check_dim((values.len() - 1) as i64)?;
    let mut x = Vec::with_capacity(values.len() - 1);
    for v in &values[1..] {
        #[allow(clippy::cast_possible_truncation)]
        let v = (v / n) as f32;
        check_element(v)?;
        x.push(v);
    }
    Ok(encode(&x))
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
        let value = encode(&[1.5, -2.0, 0.25]);
        let Value::Bytea(sent) =
            session.call(vector_send, &[0], oid::BYTEA, std::slice::from_ref(&value)).unwrap()
        else {
            panic!("not bytea")
        };
        assert_eq!(&sent[..4], &[0, 3, 0, 0]);
        assert_eq!(&sent[4..8], &1.5f32.to_be_bytes());
        let recv = |bytes: Vec<u8>, typmod: i32| {
            session.call(
                vector_recv,
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
        assert_eq!(strtof(b"1e-46"), Some((0.0, 5, false)));
        assert_eq!(strtof(b"."), None);
        assert_eq!(strtof(b"abc"), None);
        assert_eq!(strtof(b"-Infinity").map(|(v, n, _)| (v, n)), Some((f32::NEG_INFINITY, 9)));
    }
}
