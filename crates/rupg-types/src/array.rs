//! Arrays in the text and the binary format, and `int2vector` and `oidvector`.
//!
//! A port of `array_in`, `array_out`, `array_recv` and `array_send` in `src/backend/utils/adt/arrayfuncs.c`. The functions are generic over the element: the caller gives the input, the output, the receive or the send function of the element type, with the typmod in the closure. The text input calls the element input in the order of the elements, so the first error is the error of PostgreSQL also when a later part of the string is malformed.
//!
//! Lifted from `crates/rudb-pgtypes/src/array.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use rupg_common::SqlState;

use crate::binary::Recv;
use crate::error::TypeError;
use crate::number::{int_out, is_space, uint32_in_subr};
use crate::oid;
use crate::types::{Oid, format_type};

/// The maximum number of dimensions of an array.
pub const MAXDIM: usize = 6;

/// The maximum number of elements of an array, `MaxArraySize`.
pub const MAX_ARRAY_SIZE: usize = 134_217_727;

/// The first OID that `initdb` gives to an object that is not in the catalog headers.
const FIRST_GENBKI_OBJECT_ID: Oid = 10_000;

/// One dimension of an array: the number of elements and the lower bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArrayDim {
    pub len: i32,
    pub lower: i32,
}

/// An array value. The elements are in row-major order, and `None` is a null element. An empty array has no dimensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Array<T> {
    pub dims: Vec<ArrayDim>,
    pub values: Vec<Option<T>>,
}

impl<T> Array<T> {
    pub fn empty() -> Array<T> {
        Array { dims: Vec::new(), values: Vec::new() }
    }

    /// A one-dimensional array with the lower bound 1, or the empty array when there are no values.
    pub fn one(values: Vec<Option<T>>) -> Array<T> {
        if values.is_empty() {
            return Array::empty();
        }
        let len = i32::try_from(values.len()).unwrap_or(i32::MAX);
        Array { dims: vec![ArrayDim { len, lower: 1 }], values }
    }

    /// The array of an `int2vector` or an `oidvector`: one dimension with the lower bound 0, or the empty array when there are no values.
    pub fn vector(values: Vec<Option<T>>) -> Array<T> {
        if values.is_empty() {
            return Array::empty();
        }
        let len = i32::try_from(values.len()).unwrap_or(i32::MAX);
        Array { dims: vec![ArrayDim { len, lower: 0 }], values }
    }
}

fn malformed(input: &str, detail: &str) -> TypeError {
    let mut error = TypeError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("malformed array literal: \"{input}\""),
    );
    error.detail = Some(detail.to_string());
    error
}

fn limit(message: String) -> TypeError {
    TypeError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, message)
}

fn too_large() -> TypeError {
    limit(format!("array size exceeds the maximum allowed ({MAX_ARRAY_SIZE})"))
}

fn too_many_dimensions() -> TypeError {
    limit(format!("number of array dimensions exceeds the maximum allowed ({MAXDIM})"))
}

fn binary(message: String) -> TypeError {
    TypeError::new(SqlState::INVALID_BINARY_REPRESENTATION, message)
}

/// The number of elements of an array with these dimensions, `ArrayGetNItems`.
fn item_count(dims: &[ArrayDim]) -> Result<usize, TypeError> {
    let mut count = 1i32;
    for dim in dims {
        // A negative length is an upper bound that overflowed.
        if dim.len < 0 {
            return Err(too_large());
        }
        count = count.checked_mul(dim.len).ok_or_else(too_large)?;
    }
    let count = if dims.is_empty() { 0 } else { count as usize };
    if count > MAX_ARRAY_SIZE {
        return Err(too_large());
    }
    Ok(count)
}

/// `ReadDimensionInt`: an integer with an optional sign and no white space before it. The end is the start when there are no digits.
fn dimension_int(b: &[u8], start: usize) -> Result<(i32, usize), TypeError> {
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut i = start;
    let neg = at(i) == b'-';
    if neg || at(i) == b'+' {
        i += 1;
    }
    let first = i;
    let mut value = 0i64;
    while at(i).is_ascii_digit() {
        value = value.saturating_mul(10).saturating_add(i64::from(at(i) - b'0'));
        i += 1;
    }
    if i == first {
        return Ok((0, start));
    }
    let value = if neg { -value } else { value };
    let value = i32::try_from(value)
        .map_err(|_| limit("array bound is out of integer range".to_string()))?;
    Ok((value, i))
}

/// A token of the part in braces, `ArrayToken`.
enum Token {
    Start,
    End,
    Delim,
    Element,
    Null,
}

/// The reader of the part in braces. The text of the last element is in `buf`.
struct Reader<'a> {
    input: &'a str,
    b: &'a [u8],
    at: usize,
    delim: u8,
    array_nulls: bool,
    buf: Vec<u8>,
}

impl Reader<'_> {
    fn byte(&self, i: usize) -> u8 {
        self.b.get(i).copied().unwrap_or(0)
    }

    fn malformed(&self, detail: &str) -> TypeError {
        malformed(self.input, detail)
    }

    fn unexpected(&self, c: u8) -> TypeError {
        self.malformed(&format!("Unexpected \"{}\" character.", char::from(c)))
    }

    fn end(&self) -> TypeError {
        self.malformed("Unexpected end of input.")
    }

    /// `ReadArrayToken`.
    fn token(&mut self) -> Result<Token, TypeError> {
        self.buf.clear();
        let mut p = self.at;
        loop {
            match self.byte(p) {
                0 => return Err(self.end()),
                b'{' => {
                    self.at = p + 1;
                    return Ok(Token::Start);
                }
                b'}' => {
                    self.at = p + 1;
                    return Ok(Token::End);
                }
                b'"' => return self.quoted(p + 1),
                c if c == self.delim => {
                    self.at = p + 1;
                    return Ok(Token::Delim);
                }
                c if is_space(c) => p += 1,
                _ => return self.unquoted(p),
            }
        }
    }

    fn quoted(&mut self, mut p: usize) -> Result<Token, TypeError> {
        loop {
            match self.byte(p) {
                0 => return Err(self.end()),
                b'\\' => {
                    p += 1;
                    match self.byte(p) {
                        0 => return Err(self.end()),
                        c => self.buf.push(c),
                    }
                    p += 1;
                }
                b'"' => loop {
                    // Only white space can come between the quote and the next token.
                    p += 1;
                    match self.byte(p) {
                        0 => return Err(self.end()),
                        c if c == self.delim || c == b'}' || c == b'{' => {
                            self.at = p;
                            return Ok(Token::Element);
                        }
                        c if !is_space(c) => {
                            return Err(self.malformed("Incorrectly quoted array element."));
                        }
                        _ => {}
                    }
                },
                c => {
                    self.buf.push(c);
                    p += 1;
                }
            }
        }
    }

    fn unquoted(&mut self, mut p: usize) -> Result<Token, TypeError> {
        // The white space at the end is not part of the element, and `keep` is the length without it.
        let mut keep = 0;
        let mut escapes = false;
        loop {
            match self.byte(p) {
                0 => return Err(self.end()),
                b'{' => return Err(self.unexpected(b'{')),
                b'"' => return Err(self.malformed("Incorrectly quoted array element.")),
                b'\\' => {
                    p += 1;
                    match self.byte(p) {
                        0 => return Err(self.end()),
                        c => self.buf.push(c),
                    }
                    p += 1;
                    keep = self.buf.len();
                    escapes = true;
                }
                c if c == self.delim || c == b'}' => {
                    self.buf.truncate(keep);
                    self.at = p;
                    if self.array_nulls && !escapes && self.buf.eq_ignore_ascii_case(b"NULL") {
                        return Ok(Token::Null);
                    }
                    return Ok(Token::Element);
                }
                c => {
                    self.buf.push(c);
                    if !is_space(c) {
                        keep = self.buf.len();
                    }
                    p += 1;
                }
            }
        }
    }

    /// The text of the last element. The reader removes only ASCII bytes from a UTF-8 string, and it cuts only after a byte that is not white space, so the text is UTF-8.
    fn element(&self) -> &str {
        std::str::from_utf8(&self.buf).unwrap_or_default()
    }
}

/// The text input of an array: `{1,2,3}`, `{{1,2},{3,4}}` or `[0:1]={a,b}`. `delim` is the `typdelim` of the element type, and `array_nulls` is the setting of the same name: when it is off, an unquoted `NULL` is the string and not a null. `element` is the input function of the element type. It does not get the null elements.
pub fn array_in<T>(
    s: &str,
    delim: u8,
    array_nulls: bool,
    mut element: impl FnMut(&str) -> Result<T, TypeError>,
) -> Result<Array<T>, TypeError> {
    let b = s.as_bytes();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut p = 0;

    // ReadArrayDimensions: zero or more `[n]` or `[m:n]`, with white space between them.
    let mut dims: Vec<ArrayDim> = Vec::new();
    loop {
        while is_space(at(p)) {
            p += 1;
        }
        if at(p) != b'[' {
            break;
        }
        p += 1;
        if dims.len() >= MAXDIM {
            return Err(too_many_dimensions());
        }
        let (first, end) = dimension_int(b, p)?;
        if end == p {
            return Err(malformed(
                s,
                "\"[\" must introduce explicitly-specified array dimensions.",
            ));
        }
        p = end;
        let (lower, upper) = if at(p) == b':' {
            p += 1;
            let (upper, end) = dimension_int(b, p)?;
            if end == p {
                return Err(malformed(s, "Missing array dimension value."));
            }
            p = end;
            (first, upper)
        } else {
            (1, first)
        };
        if at(p) != b']' {
            return Err(malformed(s, "Missing \"]\" after array dimensions."));
        }
        p += 1;
        if upper < lower {
            return Err(TypeError::new(
                SqlState::ARRAY_SUBSCRIPT_ERROR,
                "upper bound cannot be less than lower bound".to_string(),
            ));
        }
        if upper == i32::MAX {
            return Err(limit(format!("array upper bound is too large: {upper}")));
        }
        let len = upper.checked_sub(lower).and_then(|n| n.checked_add(1)).ok_or_else(too_large)?;
        dims.push(ArrayDim { len, lower });
    }
    if dims.is_empty() {
        if at(p) != b'{' {
            return Err(malformed(
                s,
                "Array value must start with \"{\" or dimension information.",
            ));
        }
    } else {
        if at(p) != b'=' {
            return Err(malformed(s, "Missing \"=\" after array dimensions."));
        }
        p += 1;
        while is_space(at(p)) {
            p += 1;
        }
        if at(p) != b'{' {
            return Err(malformed(s, "Array contents must start with \"{\"."));
        }
    }

    // ReadArrayStr: the elements, and the dimensions when the input does not give them.
    let specified = !dims.is_empty();
    let mut ndim = dims.len();
    let mut lens = [-1i32; MAXDIM];
    for (len, dim) in lens.iter_mut().zip(&dims) {
        *len = dim.len;
    }
    let mut counts = [0i32; MAXDIM];
    let mut values = Vec::new();
    let mut level = 0;
    let mut frozen = specified;
    let mut expect_delim = false;
    let mut reader = Reader { input: s, b, at: p, delim, array_nulls, buf: Vec::new() };
    let dimension_error = || match specified {
        true => malformed(s, "Specified array dimensions do not match array contents."),
        false => {
            malformed(s, "Multidimensional arrays must have sub-arrays with matching dimensions.")
        }
    };
    loop {
        match reader.token()? {
            Token::Start => {
                if expect_delim {
                    return Err(reader.unexpected(b'{'));
                }
                if level >= MAXDIM {
                    return Err(too_many_dimensions());
                }
                counts[level] = 0;
                level += 1;
                if level > ndim {
                    if frozen {
                        return Err(dimension_error());
                    }
                    ndim = level;
                }
            }
            Token::End => {
                // A right brace can end an empty sub-array. Else it must come where a delimiter can come.
                if counts[level - 1] > 0 && !expect_delim {
                    return Err(reader.unexpected(b'}'));
                }
                level -= 1;
                if level > 0 {
                    counts[level - 1] += 1;
                }
                if lens[level] < 0 {
                    lens[level] = counts[level];
                } else if counts[level] != lens[level] {
                    return Err(dimension_error());
                }
                expect_delim = true;
            }
            Token::Delim => {
                if !expect_delim {
                    return Err(reader.unexpected(delim));
                }
                expect_delim = false;
            }
            token @ (Token::Element | Token::Null) => {
                if expect_delim {
                    return Err(reader.malformed("Unexpected array element."));
                }
                if values.len() >= MAX_ARRAY_SIZE {
                    return Err(too_large());
                }
                values.push(match token {
                    Token::Null => None,
                    _ => Some(element(reader.element())?),
                });
                // After the first element the number of dimensions is fixed, and each element must be at the last level.
                frozen = true;
                if level != ndim {
                    return Err(dimension_error());
                }
                counts[level - 1] += 1;
                expect_delim = true;
            }
        }
        if level == 0 {
            break;
        }
    }
    if b[reader.at..].iter().any(|&c| !is_space(c)) {
        return Err(malformed(s, "Junk after closing right brace."));
    }
    if values.is_empty() {
        return Ok(Array::empty());
    }
    let lowers = dims.iter().map(|dim| dim.lower).chain(std::iter::repeat(1));
    let dims = lens[..ndim].iter().zip(lowers).map(|(&len, lower)| ArrayDim { len, lower });
    Ok(Array { dims: dims.collect(), values })
}

/// The text output of an array. `element` is the output function of the element type. The function quotes an element that is empty, that is `NULL` in any case, or that has a quote, a backslash, a brace, the delimiter or white space.
pub fn array_out<T>(
    array: &Array<T>,
    delim: u8,
    out: &mut Vec<u8>,
    mut element: impl FnMut(&T, &mut Vec<u8>),
) {
    let ndim = array.dims.len();
    if array.values.is_empty() || ndim == 0 {
        out.extend_from_slice(b"{}");
        return;
    }
    if array.dims.iter().any(|dim| dim.lower != 1) {
        for dim in &array.dims {
            out.push(b'[');
            int_out(dim.lower.into(), out);
            out.push(b':');
            int_out(i64::from(dim.lower) + i64::from(dim.len) - 1, out);
            out.push(b']');
        }
        out.push(b'=');
    }
    out.push(b'{');
    let mut index = [0i32; MAXDIM];
    let mut open = 0;
    for value in &array.values {
        out.extend(std::iter::repeat_n(b'{', ndim - 1 - open));
        match value {
            None => out.extend_from_slice(b"NULL"),
            Some(value) => {
                // The element goes to the end of the output, and moves into quotes only when it needs them.
                let start = out.len();
                element(value, out);
                let text = &out[start..];
                let quote = text.is_empty()
                    || text.eq_ignore_ascii_case(b"NULL")
                    || text.iter().any(|&c| {
                        matches!(c, b'"' | b'\\' | b'{' | b'}') || c == delim || is_space(c)
                    });
                if quote {
                    let text = out.split_off(start);
                    out.push(b'"');
                    for c in text {
                        if c == b'"' || c == b'\\' {
                            out.push(b'\\');
                        }
                        out.push(c);
                    }
                    out.push(b'"');
                }
            }
        }
        // Close each dimension that is now full, and write the delimiter in the first that is not.
        open = ndim;
        for i in (0..ndim).rev() {
            index[i] += 1;
            if index[i] < array.dims[i].len {
                out.push(delim);
                open = i;
                break;
            }
            index[i] = 0;
            out.push(b'}');
        }
        if open == ndim {
            break;
        }
    }
}

/// The binary input of an array. `elem` is the element type that the column or the parameter has, and `element` is the receive function of that type, which gets the bytes of one element.
pub fn array_recv<'a, T>(
    recv: &mut Recv<'a>,
    elem: Oid,
    mut element: impl FnMut(&mut Recv<'a>) -> Result<T, TypeError>,
) -> Result<Array<T>, TypeError> {
    let ndim = recv.i32()?;
    if ndim < 0 {
        return Err(binary(format!("invalid number of dimensions: {ndim}")));
    }
    if ndim as usize > MAXDIM {
        return Err(limit(format!(
            "number of array dimensions ({ndim}) exceeds the maximum allowed ({MAXDIM})"
        )));
    }
    let flags = recv.i32()?;
    if flags != 0 && flags != 1 {
        return Err(binary("invalid array flags".to_string()));
    }
    // Only the OIDs of the built-in types are the same on each server, so a different OID is an error only when both are built-in. Else the data is read as the expected type.
    let got = recv.u32()?;
    if got != elem && got < FIRST_GENBKI_OBJECT_ID && elem < FIRST_GENBKI_OBJECT_ID {
        return Err(TypeError::new(
            SqlState::DATATYPE_MISMATCH,
            format!(
                "binary data has array element type {got} ({}) instead of expected {elem} ({})",
                format_type(got),
                format_type(elem)
            ),
        ));
    }
    let mut dims = Vec::with_capacity(ndim as usize);
    for _ in 0..ndim {
        let len = recv.i32()?;
        let lower = recv.i32()?;
        dims.push(ArrayDim { len, lower });
    }
    let count = item_count(&dims)?;
    for dim in &dims {
        if dim.len.checked_add(dim.lower).is_none() {
            return Err(limit(format!("array lower bound is too large: {}", dim.lower)));
        }
    }
    if count == 0 {
        return Ok(Array::empty());
    }
    let mut values = Vec::with_capacity(count.min(recv.remaining() / 4));
    for number in 1..=count {
        let len = recv.i32()?;
        if len < -1 || len > i32::try_from(recv.remaining()).unwrap_or(i32::MAX) {
            return Err(binary("insufficient data left in message".to_string()));
        }
        if len == -1 {
            values.push(None);
            continue;
        }
        let mut item = Recv::new(recv.bytes(len as usize)?);
        values.push(Some(element(&mut item)?));
        if item.remaining() != 0 {
            return Err(binary(format!("improper binary format in array element {number}")));
        }
    }
    Ok(Array { dims, values })
}

/// The binary output of an array. `elem` is the OID of the element type, and `element` is the send function of that type.
pub fn array_send<T>(
    array: &Array<T>,
    elem: Oid,
    out: &mut Vec<u8>,
    mut element: impl FnMut(&T, &mut Vec<u8>),
) {
    let ndim = i32::try_from(array.dims.len()).unwrap_or(i32::MAX);
    out.extend_from_slice(&ndim.to_be_bytes());
    let nulls = array.values.iter().any(Option::is_none);
    out.extend_from_slice(&i32::from(nulls).to_be_bytes());
    out.extend_from_slice(&elem.to_be_bytes());
    for dim in &array.dims {
        out.extend_from_slice(&dim.len.to_be_bytes());
        out.extend_from_slice(&dim.lower.to_be_bytes());
    }
    for value in &array.values {
        let Some(value) = value else {
            out.extend_from_slice(&(-1i32).to_be_bytes());
            continue;
        };
        let at = out.len();
        out.extend_from_slice(&[0; 4]);
        element(value, out);
        let len = i32::try_from(out.len() - at - 4).unwrap_or(i32::MAX);
        out[at..at + 4].copy_from_slice(&len.to_be_bytes());
    }
}

/// The text input of `int2vector`: integers between white space. The input reads each number with `strtol`, and an error shows the rest of the string from the bad number.
pub fn int2vector_in(s: &str) -> Result<Vec<i16>, TypeError> {
    let b = s.as_bytes();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut values = Vec::new();
    let mut p = 0;
    loop {
        while is_space(at(p)) {
            p += 1;
        }
        if p == b.len() {
            return Ok(values);
        }
        let rest = &s[p..];
        let mut i = p;
        let neg = at(i) == b'-';
        if neg || at(i) == b'+' {
            i += 1;
        }
        let first = i;
        let mut value = 0i64;
        let mut overflow = false;
        while at(i).is_ascii_digit() {
            let digit = i64::from(at(i) - b'0');
            match value.checked_mul(10).and_then(|v| v.checked_add(digit)) {
                Some(v) => value = v,
                None => overflow = true,
            }
            i += 1;
        }
        if i == first {
            return Err(TypeError::syntax("smallint", rest));
        }
        let value = match i16::try_from(if neg { -value } else { value }) {
            Ok(value) if !overflow => value,
            _ => return Err(TypeError::range(rest, "smallint")),
        };
        if i < b.len() && at(i) != b' ' {
            return Err(TypeError::syntax("smallint", rest));
        }
        values.push(value);
        p = i;
    }
}

/// The text output of `int2vector`: the numbers with a space between them.
pub fn int2vector_out(values: &[i16], out: &mut Vec<u8>) {
    for (i, &value) in values.iter().enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        int_out(value.into(), out);
    }
}

/// The binary input of `int2vector`: an array of `int2` with one dimension, the lower bound 0 and no null. An empty value is an error, because it has no dimension.
pub fn int2vector_recv(recv: &mut Recv<'_>) -> Result<Vec<i16>, TypeError> {
    vector_recv(recv, oid::INT2, Recv::i16, "int2vector")
}

/// The binary output of `int2vector`.
pub fn int2vector_send(values: &[i16], out: &mut Vec<u8>) {
    vector_send(values, oid::INT2, out, |v, out| out.extend_from_slice(&v.to_be_bytes()));
}

/// The text input of `oidvector`. Each number is read as the input of `oid` reads it, but the next number can follow with no space.
pub fn oidvector_in(s: &str) -> Result<Vec<u32>, TypeError> {
    let b = s.as_bytes();
    let mut values = Vec::new();
    let mut p = 0;
    loop {
        while p < b.len() && is_space(b[p]) {
            p += 1;
        }
        if p == b.len() {
            return Ok(values);
        }
        let (value, len) = uint32_in_subr(&s[p..], "oid")?;
        values.push(value);
        p += len;
    }
}

/// The text output of `oidvector`.
pub fn oidvector_out(values: &[u32], out: &mut Vec<u8>) {
    for (i, &value) in values.iter().enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        int_out(value.into(), out);
    }
}

/// The binary input of `oidvector`, with the rules of [`int2vector_recv`].
pub fn oidvector_recv(recv: &mut Recv<'_>) -> Result<Vec<u32>, TypeError> {
    vector_recv(recv, oid::OID, Recv::u32, "oidvector")
}

/// The binary output of `oidvector`.
pub fn oidvector_send(values: &[u32], out: &mut Vec<u8>) {
    vector_send(values, oid::OID, out, |v, out| out.extend_from_slice(&v.to_be_bytes()));
}

fn vector_recv<'a, T>(
    recv: &mut Recv<'a>,
    elem: Oid,
    element: impl FnMut(&mut Recv<'a>) -> Result<T, TypeError>,
    type_name: &str,
) -> Result<Vec<T>, TypeError> {
    let array = array_recv(recv, elem, element)?;
    let valid = matches!(array.dims[..], [ArrayDim { lower: 0, .. }]);
    if !valid || array.values.iter().any(Option::is_none) {
        return Err(binary(format!("invalid {type_name} data")));
    }
    Ok(array.values.into_iter().flatten().collect())
}

fn vector_send<T: Copy>(
    values: &[T],
    elem: Oid,
    out: &mut Vec<u8>,
    mut element: impl FnMut(T, &mut Vec<u8>),
) {
    // A vector has one dimension also when it is empty.
    let len = i32::try_from(values.len()).unwrap_or(i32::MAX);
    for word in [1, 0, elem as i32, len, 0] {
        out.extend_from_slice(&word.to_be_bytes());
    }
    for &value in values {
        let at = out.len();
        out.extend_from_slice(&[0; 4]);
        element(value, out);
        let len = i32::try_from(out.len() - at - 4).unwrap_or(i32::MAX);
        out[at..at + 4].copy_from_slice(&len.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int4s(s: &str) -> Result<Array<i32>, TypeError> {
        array_in(s, b',', true, crate::int4_in)
    }

    fn text(array: &Array<String>) -> String {
        let mut out = Vec::new();
        array_out(array, b',', &mut out, |v, out| out.extend_from_slice(v.as_bytes()));
        String::from_utf8(out).unwrap()
    }

    fn texts(s: &str) -> Array<String> {
        array_in(s, b',', true, |v| Ok(v.to_string())).unwrap()
    }

    #[test]
    fn the_text_form_reads_the_dimensions_from_the_braces() {
        let array = int4s(" {{1,2},{3,NULL}} ").unwrap();
        let dims = [ArrayDim { len: 2, lower: 1 }, ArrayDim { len: 2, lower: 1 }];
        assert_eq!(array.dims, dims);
        assert_eq!(array.values, [Some(1), Some(2), Some(3), None]);
        assert_eq!(int4s("{}"), Ok(Array::empty()));
        assert_eq!(int4s("{{},{}}"), Ok(Array::empty()));
        let array = int4s("[0:1]={7,8}").unwrap();
        assert_eq!(array.dims, [ArrayDim { len: 2, lower: 0 }]);

        let detail = |s: &str| int4s(s).unwrap_err().detail.unwrap();
        assert_eq!(
            detail("{1,{2}}"),
            "Multidimensional arrays must have sub-arrays with matching dimensions."
        );
        assert_eq!(detail("{1,}"), "Unexpected \"}\" character.");
        assert_eq!(detail("{1,,2}"), "Unexpected \",\" character.");
        assert_eq!(detail("{{1}{2}}"), "Unexpected \"{\" character.");
        assert_eq!(
            detail("{{1},2}"),
            "Multidimensional arrays must have sub-arrays with matching dimensions."
        );
        assert_eq!(
            detail("{{1},{2,3}}"),
            "Multidimensional arrays must have sub-arrays with matching dimensions."
        );
        assert_eq!(detail("[1:2]={1}"), "Specified array dimensions do not match array contents.");
        assert_eq!(detail("{1"), "Unexpected end of input.");
        assert_eq!(detail("{1} x"), "Junk after closing right brace.");
        assert_eq!(detail("{\"1\"x}"), "Incorrectly quoted array element.");
        assert_eq!(detail("{\"1\" \"2\"}"), "Incorrectly quoted array element.");
        assert_eq!(detail("{1\"2\"}"), "Incorrectly quoted array element.");
        assert_eq!(detail("[1:2"), "Missing \"]\" after array dimensions.");
        assert_eq!(detail("[1:]={1}"), "Missing array dimension value.");
        assert_eq!(
            detail("[x]={1}"),
            "\"[\" must introduce explicitly-specified array dimensions."
        );
        assert_eq!(detail("[1]{1}"), "Missing \"=\" after array dimensions.");
        assert_eq!(detail("[1]= 1"), "Array contents must start with \"{\".");
        assert_eq!(detail("1"), "Array value must start with \"{\" or dimension information.");
        assert_eq!(
            int4s("{1 2}").unwrap_err().message,
            "invalid input syntax for type integer: \"1 2\""
        );
        assert_eq!(int4s("[2:1]={}").unwrap_err().sqlstate.as_str(), "2202E");
        assert_eq!(
            int4s("[1:2147483647]={}").unwrap_err().message,
            "array upper bound is too large: 2147483647"
        );
        assert_eq!(
            int4s("[1:99999999999]={}").unwrap_err().message,
            "array bound is out of integer range"
        );
        // The element input runs in order, so its error comes before the syntax error after it.
        assert_eq!(
            int4s("{x,}").unwrap_err().message,
            "invalid input syntax for type integer: \"x\""
        );
        let deep = format!("{}1{}", "{".repeat(7), "}".repeat(7));
        assert_eq!(int4s(&deep).unwrap_err().sqlstate.as_str(), "54000");
    }

    #[test]
    fn the_output_quotes_only_what_the_input_needs() {
        let array = texts(r#"{"", "NULL", a b, "x\"y", plain, NULL}"#);
        assert_eq!(text(&array), r#"{"","NULL","a b","x\"y",plain,NULL}"#);
        assert_eq!(texts(&text(&array)), array);
        assert_eq!(text(&texts("[-1:0][2:2]={{a},{b}}")), "[-1:0][2:2]={{a},{b}}");
        assert_eq!(text(&texts("{{a,b},{c,d}}")), "{{a,b},{c,d}}");
        let off = array_in("{NULL}", b',', false, |v| Ok(v.to_string())).unwrap();
        assert_eq!(off.values, [Some("NULL".to_string())]);
    }

    #[test]
    fn the_binary_form_goes_back_to_the_same_array() {
        let array = int4s("[0:2]={1,NULL,3}").unwrap();
        let mut bytes = Vec::new();
        array_send(&array, oid::INT4, &mut bytes, |v, out| out.extend_from_slice(&v.to_be_bytes()));
        let back = array_recv(&mut Recv::new(&bytes), oid::INT4, Recv::i32).unwrap();
        assert_eq!(back, array);

        let error = array_recv(&mut Recv::new(&bytes), oid::INT2, Recv::i16).unwrap_err();
        assert_eq!(
            error.message,
            "binary data has array element type 23 (integer) instead of expected 21 (smallint)"
        );
        let error = array_recv(&mut Recv::new(&bytes), oid::INT8, Recv::i64).unwrap_err();
        assert_eq!(error.sqlstate.as_str(), "42804");
        // Element 1 has four bytes, and a receive function that reads two leaves two.
        let mut words = bytes.clone();
        words[8..12].copy_from_slice(&oid::INT2.to_be_bytes());
        let error = array_recv(&mut Recv::new(&words), oid::INT2, Recv::i16).unwrap_err();
        assert_eq!(error.message, "improper binary format in array element 1");

        let header =
            |words: &[i32]| -> Vec<u8> { words.iter().flat_map(|w| w.to_be_bytes()).collect() };
        let message = |bytes: Vec<u8>| {
            array_recv(&mut Recv::new(&bytes), oid::INT4, Recv::i32).unwrap_err().message
        };
        assert_eq!(message(header(&[-1])), "invalid number of dimensions: -1");
        assert_eq!(
            message(header(&[7])),
            "number of array dimensions (7) exceeds the maximum allowed (6)"
        );
        assert_eq!(message(header(&[1, 2])), "invalid array flags");
        assert_eq!(
            message(header(&[1, 0, 23, -1, 1])),
            "array size exceeds the maximum allowed (134217727)"
        );
        assert_eq!(
            message(header(&[1, 0, 23, 1, i32::MAX])),
            "array lower bound is too large: 2147483647"
        );
        assert_eq!(message(header(&[1, 0, 23, 1, 1, 5, 0])), "insufficient data left in message");
        let empty = header(&[1, 0, 23, 0, 1]);
        assert_eq!(array_recv(&mut Recv::new(&empty), oid::INT4, Recv::i32), Ok(Array::empty()));
    }

    #[test]
    fn a_vector_is_a_zero_based_array_with_spaces_in_the_text() {
        assert_eq!(int2vector_in(" 1  -2 3 "), Ok(vec![1, -2, 3]));
        assert_eq!(int2vector_in(""), Ok(vec![]));
        assert_eq!(
            int2vector_in("1 x").unwrap_err().message,
            "invalid input syntax for type smallint: \"x\""
        );
        assert_eq!(
            int2vector_in("1,2").unwrap_err().message,
            "invalid input syntax for type smallint: \"1,2\""
        );
        assert_eq!(
            int2vector_in("40000 1").unwrap_err().message,
            "value \"40000 1\" is out of range for type smallint"
        );
        assert_eq!(oidvector_in("1-2 0x10"), Ok(vec![1, u32::MAX - 1, 16]));
        assert_eq!(
            oidvector_in("1 x").unwrap_err().message,
            "invalid input syntax for type oid: \"x\""
        );

        let mut bytes = Vec::new();
        int2vector_send(&[5, 6], &mut bytes);
        assert_eq!(int2vector_recv(&mut Recv::new(&bytes)), Ok(vec![5, 6]));
        let mut out = Vec::new();
        int2vector_out(&[5, -6], &mut out);
        assert_eq!(out, b"5 -6");

        let mut empty = Vec::new();
        oidvector_send(&[], &mut empty);
        let error = oidvector_recv(&mut Recv::new(&empty)).unwrap_err();
        assert_eq!(
            (error.sqlstate.as_str(), error.message.as_str()),
            ("22P03", "invalid oidvector data")
        );
    }
}
