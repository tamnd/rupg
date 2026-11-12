//! The type `sparsevec` of pgvector: a port of `src/sparsevec.c` of pgvector 0.8.7.
//!
//! A value of `sparsevec` is the data of its varlena: the dimensions, the number of the elements that are not 0 and an `int32` that is 0, then the indices of those elements from 0 in ascending order as `int32`, then their values as `float`, in the byte order of the machine as pgvector keeps it.

use rupg_common::{Error, Result, SqlState};
use rupg_func::Call;
use rupg_types::Value;

use super::{
    Dense, array_elements, check_expected_dim, checked, cosine, data_exception, decode, encode,
    int4_arg, is_space, long_message, overflow, short_message, sign, strtof, typmod, wrong_kind,
};

/// `SPARSEVEC_MAX_DIM`.
const SPARSEVEC_MAX_DIM: i32 = 1_000_000_000;
/// `SPARSEVEC_MAX_NNZ`.
const SPARSEVEC_MAX_NNZ: i32 = 16000;

/// A value of `sparsevec`.
struct Sparse {
    dim: i32,
    indices: Vec<i32>,
    values: Vec<f32>,
}

impl Sparse {
    fn decode(value: &Value) -> Result<Sparse> {
        let Value::Bytea(data) = value else { return Err(wrong_kind()) };
        let int = |at: usize| -> Result<i32> {
            let bytes = data.get(at..at + 4).ok_or_else(wrong_kind)?;
            Ok(i32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        };
        let dim = int(0)?;
        let nnz = usize::try_from(int(4)?).map_err(|_| wrong_kind())?;
        if data.len() != 12 + 8 * nnz {
            return Err(wrong_kind());
        }
        let indices = (0..nnz).map(|i| int(12 + 4 * i)).collect::<Result<Vec<_>>>()?;
        let values = data[12 + 4 * nnz..]
            .chunks(4)
            .map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        Ok(Sparse { dim, indices, values })
    }

    /// `InitSparseVector` with the elements.
    fn encode(&self) -> Value {
        let mut data = Vec::with_capacity(12 + 8 * self.indices.len());
        data.extend_from_slice(&self.dim.to_ne_bytes());
        // The elements are at most SPARSEVEC_MAX_NNZ, so they fit.
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        data.extend_from_slice(&(self.indices.len() as i32).to_ne_bytes());
        data.extend_from_slice(&0i32.to_ne_bytes());
        for i in &self.indices {
            data.extend_from_slice(&i.to_ne_bytes());
        }
        for v in &self.values {
            data.extend_from_slice(&v.to_ne_bytes());
        }
        Value::Bytea(data)
    }

    /// The indices and the values of the elements.
    fn elements(&self) -> impl Iterator<Item = (i32, f32)> + '_ {
        self.indices.iter().copied().zip(self.values.iter().copied())
    }
}

fn one(args: &[Value]) -> Result<Sparse> {
    Sparse::decode(args.first().ok_or_else(wrong_kind)?)
}

/// `CheckDims`: the two values of a function of two values of the same dimensions.
fn same(args: &[Value]) -> Result<(Sparse, Sparse)> {
    let [a, b, ..] = args else { return Err(wrong_kind()) };
    let (a, b) = (Sparse::decode(a)?, Sparse::decode(b)?);
    if a.dim != b.dim {
        return Err(data_exception(format!(
            "different sparsevec dimensions {} and {}",
            a.dim, b.dim
        )));
    }
    Ok((a, b))
}

/// `CheckDim`.
fn check_dim(dim: i64) -> Result<()> {
    if dim < 1 {
        return Err(data_exception("sparsevec must have at least 1 dimension"));
    }
    if dim > i64::from(SPARSEVEC_MAX_DIM) {
        return Err(Error::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("sparsevec cannot have more than {SPARSEVEC_MAX_DIM} dimensions"),
        ));
    }
    Ok(())
}

fn too_many() -> Error {
    Error::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("sparsevec cannot have more than {SPARSEVEC_MAX_NNZ} non-zero elements"),
    )
}

/// `CheckNnz`.
fn check_nnz(nnz: i64, dim: i64) -> Result<()> {
    if nnz < 0 {
        return Err(data_exception("sparsevec cannot have negative number of elements"));
    }
    if nnz > i64::from(SPARSEVEC_MAX_NNZ) {
        return Err(too_many());
    }
    if nnz > dim {
        return Err(Error::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            "sparsevec cannot have more elements than dimensions",
        ));
    }
    Ok(())
}

/// `CheckIndex` of the element `i`.
fn check_index(indices: &[i32], i: usize, dim: i32) -> Result<()> {
    let index = indices[i];
    if index < 0 || index >= dim {
        return Err(data_exception("sparsevec index out of bounds"));
    }
    if i > 0 {
        if index < indices[i - 1] {
            return Err(data_exception("sparsevec indices must be in ascending order"));
        }
        if index == indices[i - 1] {
            return Err(data_exception("sparsevec indices must not contain duplicates"));
        }
    }
    Ok(())
}

/// `CheckElement`.
fn check_element(value: f32) -> Result<()> {
    if value.is_nan() {
        return Err(data_exception("NaN not allowed in sparsevec"));
    }
    if value.is_infinite() {
        return Err(data_exception("infinite value not allowed in sparsevec"));
    }
    Ok(())
}

/// `strtol` in base 10 on the start of `s`, kept in the range of `i64`: the value and the number of bytes that it read. `None` when `s` does not start with a number.
fn strtol(s: &[u8]) -> Option<(i64, usize)> {
    let mut i = 0;
    while s.get(i).copied().is_some_and(is_space) {
        i += 1;
    }
    let negative = s.get(i) == Some(&b'-');
    if matches!(s.get(i), Some(b'+' | b'-')) {
        i += 1;
    }
    let start = i;
    let mut value: i64 = 0;
    while let Some(c) = s.get(i).filter(|c| c.is_ascii_digit()) {
        let d = i64::from(c - b'0');
        value = if negative {
            value.saturating_mul(10).saturating_sub(d)
        } else {
            value.saturating_mul(10).saturating_add(d)
        };
        i += 1;
    }
    (i > start).then_some((value, i))
}

pub(super) fn sparsevec_in(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Some(Value::Text(lit)), Ok(typmod)) = (args.first(), int4_arg(args, 2)) else {
        return Err(wrong_kind());
    };
    let syntax = || {
        Error::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type sparsevec: \"{lit}\""),
        )
    };
    let s = lit.as_bytes();
    let max_nnz = 1 + s.iter().filter(|c| **c == b',').count();
    if max_nnz > SPARSEVEC_MAX_NNZ as usize {
        return Err(too_many());
    }
    let mut pt = 0;
    let skip = |pt: &mut usize| {
        while s.get(*pt).copied().is_some_and(is_space) {
            *pt += 1;
        }
    };
    skip(&mut pt);
    if s.get(pt) != Some(&b'{') {
        return Err(syntax().with_detail("Vector contents must start with \"{\"."));
    }
    pt += 1;
    skip(&mut pt);
    let mut elements: Vec<(i32, f32)> = Vec::new();
    if s.get(pt) == Some(&b'}') {
        pt += 1;
    } else {
        loop {
            skip(&mut pt);
            if pt == s.len() {
                return Err(syntax());
            }
            let Some((index, len)) = strtol(&s[pt..]) else { return Err(syntax()) };
            // Kept in the range of int for the message of a later check.
            let index = index.clamp(i64::from(i32::MIN) + 1, i64::from(i32::MAX));
            pt += len;
            skip(&mut pt);
            if s.get(pt) != Some(&b':') {
                return Err(syntax());
            }
            pt += 1;
            skip(&mut pt);
            let Some((value, len, range)) = strtof(&s[pt..]) else { return Err(syntax()) };
            if range && (value == 0.0 || value.is_infinite()) {
                return Err(Error::new(
                    SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                    format!("\"{}\" is out of range for type sparsevec", &lit[pt..pt + len]),
                ));
            }
            check_element(value)?;
            if value != 0.0 {
                // From the numbering from 1 of SQL to the numbering from 0 of C.
                #[allow(clippy::cast_possible_truncation)]
                elements.push(((index - 1) as i32, value));
            }
            pt += len;
            skip(&mut pt);
            match s.get(pt) {
                Some(b',') => pt += 1,
                Some(b'}') => {
                    pt += 1;
                    break;
                }
                _ => return Err(syntax()),
            }
        }
    }
    skip(&mut pt);
    if s.get(pt) != Some(&b'/') {
        return Err(syntax().with_detail("Unexpected end of input."));
    }
    pt += 1;
    skip(&mut pt);
    let Some((dim, len)) = strtol(&s[pt..]) else { return Err(syntax()) };
    let dim = dim.clamp(i64::from(i32::MIN), i64::from(i32::MAX));
    pt += len;
    skip(&mut pt);
    if pt != s.len() {
        return Err(syntax().with_detail("Junk after closing."));
    }
    check_dim(dim)?;
    #[allow(clippy::cast_possible_truncation)]
    let dim = dim as i32;
    check_expected_dim(typmod, dim as usize)?;
    elements.sort_by_key(|e| e.0);
    let indices: Vec<i32> = elements.iter().map(|e| e.0).collect();
    for i in 0..indices.len() {
        check_index(&indices, i, dim)?;
    }
    Ok(Sparse { dim, indices, values: elements.iter().map(|e| e.1).collect() }.encode())
}

pub(super) fn sparsevec_out(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let a = one(args)?;
    let mut out = Vec::with_capacity(16 * a.indices.len() + 16);
    out.push(b'{');
    for (i, (index, value)) in a.elements().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice((i64::from(index) + 1).to_string().as_bytes());
        out.push(b':');
        rupg_types::float4_out(value, 1, &mut out);
    }
    out.extend_from_slice(b"}/");
    out.extend_from_slice(a.dim.to_string().as_bytes());
    Ok(Value::Text(String::from_utf8(out).map_err(|_| wrong_kind())?))
}

pub(super) fn sparsevec_typmod_in(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    typmod(args, "sparsevec", SPARSEVEC_MAX_DIM)
}

pub(super) fn sparsevec_recv(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Some(Value::Bytea(buf)), Ok(typmod)) = (args.first(), int4_arg(args, 2)) else {
        return Err(wrong_kind());
    };
    let mut at = 0;
    let mut next = || -> Result<[u8; 4]> {
        let bytes = buf.get(at..at + 4).ok_or_else(short_message)?;
        at += 4;
        Ok([bytes[0], bytes[1], bytes[2], bytes[3]])
    };
    let dim = i32::from_be_bytes(next()?);
    let nnz = i32::from_be_bytes(next()?);
    let unused = i32::from_be_bytes(next()?);
    check_dim(i64::from(dim))?;
    check_nnz(i64::from(nnz), i64::from(dim))?;
    check_expected_dim(typmod, dim as usize)?;
    if unused != 0 {
        return Err(data_exception(format!("expected unused to be 0, not {unused}")));
    }
    let nnz = nnz as usize;
    let mut indices = Vec::with_capacity(nnz);
    for i in 0..nnz {
        indices.push(i32::from_be_bytes(next()?));
        check_index(&indices, i, dim)?;
    }
    let mut values = Vec::with_capacity(nnz);
    for _ in 0..nnz {
        let value = f32::from_be_bytes(next()?);
        check_element(value)?;
        if value == 0.0 {
            return Err(data_exception(
                "binary representation of sparsevec cannot contain zero values",
            ));
        }
        values.push(value);
    }
    if buf.len() > 12 + 8 * nnz {
        return Err(long_message());
    }
    Ok(Sparse { dim, indices, values }.encode())
}

pub(super) fn sparsevec_send(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let a = one(args)?;
    let mut out = Vec::with_capacity(12 + 8 * a.indices.len());
    out.extend_from_slice(&a.dim.to_be_bytes());
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    out.extend_from_slice(&(a.indices.len() as i32).to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    for i in &a.indices {
        out.extend_from_slice(&i.to_be_bytes());
    }
    for v in &a.values {
        out.extend_from_slice(&v.to_be_bytes());
    }
    Ok(Value::Bytea(out))
}

/// `sparsevec(sparsevec, integer, boolean)`: the cast to a `sparsevec(n)`.
pub(super) fn sparsevec(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let a = one(args)?;
    check_expected_dim(int4_arg(args, 1)?, a.dim as usize)?;
    Ok(args[0].clone())
}

/// A `sparsevec` of the elements of a dense vector that are not 0.
fn from_dense(x: &[f32]) -> Result<Sparse> {
    let mut indices = Vec::new();
    let mut values = Vec::new();
    for (i, v) in x.iter().enumerate() {
        if *v != 0.0 {
            // A dense vector has at most VECTOR_MAX_DIM dimensions.
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            indices.push(i as i32);
            values.push(*v);
        }
    }
    check_nnz(indices.len() as i64, x.len() as i64)?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    Ok(Sparse { dim: x.len() as i32, indices, values })
}

/// `vector_to_sparsevec` and `halfvec_to_sparsevec`.
pub(super) fn dense_to_sparsevec<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let x = decode::<K>(args.first().ok_or_else(wrong_kind)?)?;
    check_dim(x.len() as i64)?;
    check_expected_dim(int4_arg(args, 1)?, x.len())?;
    Ok(from_dense(&x)?.encode())
}

pub(super) fn array_to_sparsevec(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Array(array)) = args.first() else { return Err(wrong_kind()) };
    let typmod = int4_arg(args, 1)?;
    if array.dims.len() <= 1 && array.values.iter().all(Option::is_some) {
        check_dim(array.values.len() as i64)?;
        check_expected_dim(typmod, array.values.len())?;
    }
    let x = array_elements(array)?;
    let a = from_dense(&x)?;
    for v in &a.values {
        check_element(*v)?;
    }
    Ok(a.encode())
}

/// `sparsevec_to_vector` and `sparsevec_to_halfvec`.
pub(super) fn sparsevec_to_dense<K: Dense>(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let a = one(args)?;
    super::check_dim::<K>(i64::from(a.dim))?;
    check_expected_dim(int4_arg(args, 1)?, a.dim as usize)?;
    let mut x = vec![0.0f32; a.dim as usize];
    for (index, value) in a.elements() {
        let slot = usize::try_from(index)
            .ok()
            .and_then(|i| x.get_mut(i))
            .ok_or_else(|| Error::internal("index out of bounds"))?;
        *slot = checked::<K>(value)?;
    }
    Ok(encode::<K>(&x))
}

/// `SparsevecL2SquaredDistance`.
fn l2_squared(a: &Sparse, b: &Sparse) -> f32 {
    let (ax, bx) = (&a.values, &b.values);
    let mut distance = 0.0f32;
    let mut bpos = 0;
    for (i, &ai) in a.indices.iter().enumerate() {
        let mut bi = -1;
        let mut j = bpos;
        while j < b.indices.len() {
            bi = b.indices[j];
            if ai == bi {
                let diff = ax[i] - bx[j];
                distance += diff * diff;
            } else if ai > bi {
                distance += bx[j] * bx[j];
            }
            if ai >= bi {
                bpos = j + 1;
            }
            if bi >= ai {
                break;
            }
            j += 1;
        }
        if ai != bi {
            distance += ax[i] * ax[i];
        }
    }
    for v in &bx[bpos..] {
        distance += v * v;
    }
    distance
}

/// `SparsevecInnerProduct`.
fn dot(a: &Sparse, b: &Sparse) -> f32 {
    let mut distance = 0.0f32;
    let mut bpos = 0;
    for (i, &ai) in a.indices.iter().enumerate() {
        let mut j = bpos;
        while j < b.indices.len() {
            let bi = b.indices[j];
            if ai == bi {
                distance += a.values[i] * b.values[j];
            }
            if ai >= bi {
                bpos = j + 1;
            }
            if bi >= ai {
                break;
            }
            j += 1;
        }
    }
    distance
}

pub(super) fn sparsevec_l2_distance(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same(args)?;
    Ok(Value::Float8(f64::from(l2_squared(&a, &b)).sqrt()))
}

pub(super) fn sparsevec_l2_squared_distance(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same(args)?;
    Ok(Value::Float8(f64::from(l2_squared(&a, &b))))
}

pub(super) fn sparsevec_inner_product(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same(args)?;
    Ok(Value::Float8(f64::from(dot(&a, &b))))
}

pub(super) fn sparsevec_negative_inner_product(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same(args)?;
    Ok(Value::Float8(f64::from(-dot(&a, &b))))
}

pub(super) fn sparsevec_cosine_distance(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same(args)?;
    let similarity = f64::from(dot(&a, &b));
    let mut norma = 0.0f32;
    for v in &a.values {
        norma += v * v;
    }
    let mut normb = 0.0f32;
    for v in &b.values {
        normb += v * v;
    }
    Ok(cosine(similarity / (f64::from(norma) * f64::from(normb)).sqrt()))
}

pub(super) fn sparsevec_l1_distance(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (a, b) = same(args)?;
    let (ax, bx) = (&a.values, &b.values);
    let mut distance = 0.0f32;
    let mut bpos = 0;
    for (i, &ai) in a.indices.iter().enumerate() {
        let mut bi = -1;
        let mut j = bpos;
        while j < b.indices.len() {
            bi = b.indices[j];
            if ai == bi {
                distance += (ax[i] - bx[j]).abs();
            } else if ai > bi {
                distance += bx[j].abs();
            }
            if ai >= bi {
                bpos = j + 1;
            }
            if bi >= ai {
                break;
            }
            j += 1;
        }
        if ai != bi {
            distance += ax[i].abs();
        }
    }
    for v in &bx[bpos..] {
        distance += v.abs();
    }
    Ok(Value::Float8(f64::from(distance)))
}

/// The norm of the values, with the sum in `double`.
fn norm(a: &Sparse) -> f64 {
    a.values.iter().fold(0.0f64, |n, v| n + f64::from(*v) * f64::from(*v)).sqrt()
}

pub(super) fn sparsevec_l2_norm(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Float8(norm(&one(args)?)))
}

pub(super) fn sparsevec_l2_normalize(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let a = one(args)?;
    let norm = norm(&a);
    if norm <= 0.0 {
        return Ok(
            Sparse { dim: a.dim, indices: a.indices, values: vec![0.0; a.values.len()] }.encode()
        );
    }
    let mut indices = Vec::with_capacity(a.indices.len());
    let mut values = Vec::with_capacity(a.values.len());
    for (index, value) in a.elements() {
        #[allow(clippy::cast_possible_truncation)]
        let r = (f64::from(value) / norm) as f32;
        if r.is_infinite() {
            return Err(overflow());
        }
        // A value that is 0 now is not kept.
        if r != 0.0 {
            indices.push(index);
            values.push(r);
        }
    }
    Ok(Sparse { dim: a.dim, indices, values }.encode())
}

/// `sparsevec_cmp_internal`.
fn cmp(args: &[Value]) -> Result<i32> {
    let [a, b, ..] = args else { return Err(wrong_kind()) };
    let (a, b) = (Sparse::decode(a)?, Sparse::decode(b)?);
    let (ax, bx) = (&a.values, &b.values);
    let nnz = a.indices.len().min(b.indices.len());
    for i in 0..nnz {
        if a.indices[i] < b.indices[i] {
            return Ok(if ax[i] < 0.0 { -1 } else { 1 });
        }
        if a.indices[i] > b.indices[i] {
            return Ok(if bx[i] < 0.0 { 1 } else { -1 });
        }
        if ax[i] < bx[i] {
            return Ok(-1);
        }
        if ax[i] > bx[i] {
            return Ok(1);
        }
    }
    if a.indices.len() < b.indices.len() && b.indices[nnz] < a.dim {
        return Ok(if bx[nnz] < 0.0 { 1 } else { -1 });
    }
    if a.indices.len() > b.indices.len() && a.indices[nnz] < b.dim {
        return Ok(if ax[nnz] < 0.0 { -1 } else { 1 });
    }
    Ok(sign(a.dim.cmp(&b.dim)))
}

pub(super) fn sparsevec_lt(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp(args)? < 0))
}

pub(super) fn sparsevec_le(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp(args)? <= 0))
}

pub(super) fn sparsevec_eq(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp(args)? == 0))
}

pub(super) fn sparsevec_ne(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp(args)? != 0))
}

pub(super) fn sparsevec_ge(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp(args)? >= 0))
}

pub(super) fn sparsevec_gt(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(cmp(args)? > 0))
}

pub(super) fn sparsevec_cmp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int4(cmp(args)?))
}
