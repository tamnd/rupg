//! The bytes of a value that the file keeps, such as the catalog of a database: [`Persist`] writes a value to bytes and reads it back.
//!
//! The form is little endian. A number of fixed width takes its width. A length, a count and a `usize` take the varint of LEB128. A `bool` takes one byte, 0 or 1. A `String` is its length and its UTF-8 bytes. An `Option` is a byte, 0 for `None` and 1 for `Some`, before the value. A `Vec`, a map and a set are the count and the items, in their order.
//!
//! [`persist_struct!`](crate::persist_struct) writes the fields of a struct in the order that the macro names them. The struct literal in the read needs every field, so a field that a change adds and that the macro does not name is a compile error. [`persist_enum!`](crate::persist_enum) writes the number that the macro gives the variant, then the fields of the variant. The match in the write needs every variant.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{BuildHasher, Hash};
use std::sync::Arc;

use crate::{Error, Result, SqlState};

/// A value that can be written to bytes and read back from them.
pub trait Persist: Sized {
    /// Appends the bytes of the value to `out`.
    fn put(&self, out: &mut Vec<u8>);
    /// Reads a value from the start of `input`, and moves `input` past its bytes.
    fn take(input: &mut Reader<'_>) -> Result<Self>;
}

/// The bytes of a value.
pub fn to_bytes<T: Persist>(value: &T) -> Vec<u8> {
    let mut out = Vec::new();
    value.put(&mut out);
    out
}

/// The value of `bytes`. Bytes after the value give an error.
pub fn from_bytes<T: Persist>(bytes: &[u8]) -> Result<T> {
    let mut input = Reader::new(bytes);
    let value = T::take(&mut input)?;
    if !input.is_empty() {
        return Err(corrupt("bytes after the end of the value"));
    }
    Ok(value)
}

/// The error of bytes that do not hold a value.
pub fn corrupt(what: &str) -> Error {
    Error::new(SqlState::DATA_CORRUPTED, format!("invalid stored value: {what}"))
}

/// The bytes that a read has not taken yet.
#[derive(Debug)]
pub struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    /// A reader at the start of `bytes`.
    pub fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes }
    }

    /// True when the read has taken all the bytes.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The next `n` bytes.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.bytes.len() {
            return Err(corrupt("the bytes end inside a value"));
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(head)
    }

    /// The next `N` bytes as an array.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }

    /// A varint of LEB128.
    pub fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let [byte] = self.array()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(corrupt("a varint is longer than 10 bytes"))
    }

    /// A length or a count. A count above the bytes that are left is an error, because each item takes at least one byte, so a bad count cannot make a large allocation.
    pub fn count(&mut self) -> Result<usize> {
        let n = usize::try_from(self.varint()?).map_err(|_| corrupt("a count is too large"))?;
        if n > self.bytes.len() {
            return Err(corrupt("a count is larger than the bytes that are left"));
        }
        Ok(n)
    }
}

/// Appends the varint of LEB128 of `value`.
pub fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Appends a length or a count.
pub fn put_count(out: &mut Vec<u8>, n: usize) {
    put_varint(out, n as u64);
}

macro_rules! fixed {
    ($($ty:ty),*) => {$(
        impl Persist for $ty {
            fn put(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }
            fn take(input: &mut Reader<'_>) -> Result<Self> {
                Ok(<$ty>::from_le_bytes(input.array()?))
            }
        }
    )*};
}

fixed!(u8, u16, u32, u64, i8, i16, i32, i64, f32, f64);

impl Persist for usize {
    fn put(&self, out: &mut Vec<u8>) {
        put_count(out, *self);
    }
    fn take(input: &mut Reader<'_>) -> Result<Self> {
        usize::try_from(input.varint()?).map_err(|_| corrupt("a usize is too large"))
    }
}

impl Persist for bool {
    fn put(&self, out: &mut Vec<u8>) {
        out.push(u8::from(*self));
    }
    fn take(input: &mut Reader<'_>) -> Result<Self> {
        match input.array()? {
            [0] => Ok(false),
            [1] => Ok(true),
            _ => Err(corrupt("a bool is not 0 or 1")),
        }
    }
}

impl Persist for char {
    fn put(&self, out: &mut Vec<u8>) {
        u32::from(*self).put(out);
    }
    fn take(input: &mut Reader<'_>) -> Result<Self> {
        char::from_u32(u32::take(input)?).ok_or_else(|| corrupt("a char is not a Unicode scalar"))
    }
}

impl Persist for String {
    fn put(&self, out: &mut Vec<u8>) {
        put_count(out, self.len());
        out.extend_from_slice(self.as_bytes());
    }
    fn take(input: &mut Reader<'_>) -> Result<Self> {
        let n = input.count()?;
        let bytes = input.bytes(n)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| corrupt("a string is not UTF-8"))
    }
}

impl<T: Persist, const N: usize> Persist for [T; N] {
    fn put(&self, out: &mut Vec<u8>) {
        for item in self {
            item.put(out);
        }
    }
    fn take(input: &mut Reader<'_>) -> Result<Self> {
        let items = (0..N).map(|_| T::take(input)).collect::<Result<Vec<T>>>()?;
        items.try_into().map_err(|_| corrupt("an array has the wrong length"))
    }
}

impl<T: Persist> Persist for Option<T> {
    fn put(&self, out: &mut Vec<u8>) {
        match self {
            None => out.push(0),
            Some(value) => {
                out.push(1);
                value.put(out);
            }
        }
    }
    fn take(input: &mut Reader<'_>) -> Result<Self> {
        match input.array()? {
            [0] => Ok(None),
            [1] => Ok(Some(T::take(input)?)),
            _ => Err(corrupt("an option is not 0 or 1")),
        }
    }
}

impl<T: Persist> Persist for Vec<T> {
    fn put(&self, out: &mut Vec<u8>) {
        put_count(out, self.len());
        for item in self {
            item.put(out);
        }
    }
    fn take(input: &mut Reader<'_>) -> Result<Self> {
        let n = input.count()?;
        (0..n).map(|_| T::take(input)).collect()
    }
}

impl<T: Persist> Persist for Box<T> {
    fn put(&self, out: &mut Vec<u8>) {
        (**self).put(out);
    }
    fn take(input: &mut Reader<'_>) -> Result<Self> {
        T::take(input).map(Box::new)
    }
}

impl<T: Persist> Persist for Box<[T]> {
    fn put(&self, out: &mut Vec<u8>) {
        put_count(out, self.len());
        for item in self.iter() {
            item.put(out);
        }
    }
    fn take(input: &mut Reader<'_>) -> Result<Self> {
        Vec::take(input).map(Vec::into_boxed_slice)
    }
}

impl<T: Persist> Persist for Arc<[T]> {
    fn put(&self, out: &mut Vec<u8>) {
        put_count(out, self.len());
        for item in self.iter() {
            item.put(out);
        }
    }
    fn take(input: &mut Reader<'_>) -> Result<Self> {
        Vec::take(input).map(Arc::from)
    }
}

impl<T: Persist> Persist for Arc<T> {
    fn put(&self, out: &mut Vec<u8>) {
        (**self).put(out);
    }
    fn take(input: &mut Reader<'_>) -> Result<Self> {
        T::take(input).map(Arc::new)
    }
}

impl<K: Persist + Ord, V: Persist> Persist for BTreeMap<K, V> {
    fn put(&self, out: &mut Vec<u8>) {
        put_count(out, self.len());
        for (key, value) in self {
            key.put(out);
            value.put(out);
        }
    }
    fn take(input: &mut Reader<'_>) -> Result<Self> {
        let n = input.count()?;
        (0..n).map(|_| Ok((K::take(input)?, V::take(input)?))).collect()
    }
}

/// The entries of a `HashMap` are in the order of the map, so two maps with the same entries can give other bytes.
impl<K: Persist + Eq + Hash, V: Persist, S: BuildHasher + Default> Persist for HashMap<K, V, S> {
    fn put(&self, out: &mut Vec<u8>) {
        put_count(out, self.len());
        for (key, value) in self {
            key.put(out);
            value.put(out);
        }
    }
    fn take(input: &mut Reader<'_>) -> Result<Self> {
        let n = input.count()?;
        (0..n).map(|_| Ok((K::take(input)?, V::take(input)?))).collect()
    }
}

impl<T: Persist + Ord> Persist for BTreeSet<T> {
    fn put(&self, out: &mut Vec<u8>) {
        put_count(out, self.len());
        for item in self {
            item.put(out);
        }
    }
    fn take(input: &mut Reader<'_>) -> Result<Self> {
        let n = input.count()?;
        (0..n).map(|_| T::take(input)).collect()
    }
}

impl Persist for () {
    fn put(&self, _: &mut Vec<u8>) {}
    fn take(_: &mut Reader<'_>) -> Result<Self> {
        Ok(())
    }
}

macro_rules! tuple {
    ($($name:ident),*) => {
        impl<$($name: Persist),*> Persist for ($($name,)*) {
            #[allow(non_snake_case)]
            fn put(&self, out: &mut Vec<u8>) {
                let ($($name,)*) = self;
                $($name.put(out);)*
            }
            fn take(input: &mut Reader<'_>) -> Result<Self> {
                Ok(($($name::take(input)?,)*))
            }
        }
    };
}

tuple!(A);
tuple!(A, B);
tuple!(A, B, C);
tuple!(A, B, C, D);

/// Implements [`Persist`] for a struct with named fields. Each field must be named once, in the order of the bytes.
///
/// ```
/// # use rupg_common::persist_struct;
/// struct Point { x: i32, y: i32 }
/// persist_struct!(Point { x, y });
/// ```
#[macro_export]
macro_rules! persist_struct {
    ($ty:ty { $($field:ident),* $(,)? }) => {
        impl $crate::persist::Persist for $ty {
            fn put(&self, out: &mut Vec<u8>) {
                $($crate::persist::Persist::put(&self.$field, out);)*
            }
            fn take(input: &mut $crate::persist::Reader<'_>) -> $crate::Result<Self> {
                Ok(Self { $($field: $crate::persist::Persist::take(input)?,)* })
            }
        }
    };
}

/// Implements [`Persist`] for an enum. Each variant has a number, which is its tag in the bytes and must not change. A variant has no fields, fields in parentheses or named fields.
///
/// ```
/// # use rupg_common::persist_enum;
/// enum Shape { Empty, Circle(u32), Box { w: u32, h: u32 } }
/// persist_enum!(Shape { 0 => Empty, 1 => Circle(r), 2 => Box { w, h } });
/// ```
#[macro_export]
macro_rules! persist_enum {
    ($ty:ident { $($tag:literal => $variant:ident $(($($pos:ident),*))? $({ $($field:ident),* })?),* $(,)? }) => {
        impl $crate::persist::Persist for $ty {
            fn put(&self, out: &mut Vec<u8>) {
                match self {
                    $($ty::$variant $(($($pos),*))? $({ $($field),* })? => {
                        out.push($tag);
                        $($($crate::persist::Persist::put($pos, out);)*)?
                        $($($crate::persist::Persist::put($field, out);)*)?
                    })*
                }
            }
            fn take(input: &mut $crate::persist::Reader<'_>) -> $crate::Result<Self> {
                let [tag] = input.array()?;
                match tag {
                    $($tag => Ok($ty::$variant
                        $(($({
                            let $pos = $crate::persist::Persist::take(input)?;
                            $pos
                        }),*))?
                        $({ $($field: $crate::persist::Persist::take(input)?),* })?),)*
                    _ => Err($crate::persist::corrupt(concat!("an unknown variant of ", stringify!($ty)))),
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Point {
        x: i32,
        name: Option<String>,
        tags: Vec<u16>,
    }
    persist_struct!(Point { x, name, tags });

    #[derive(Debug, PartialEq)]
    enum Shape {
        Empty,
        Circle(u32, bool),
        Rect { w: u64, h: f64 },
    }
    persist_enum!(Shape { 0 => Empty, 1 => Circle(r, filled), 2 => Rect { w, h } });

    fn round<T: Persist + PartialEq + std::fmt::Debug>(value: T) {
        let bytes = to_bytes(&value);
        assert_eq!(from_bytes::<T>(&bytes).unwrap(), value);
    }

    #[test]
    fn values_come_back_from_their_bytes() {
        round(Point { x: -7, name: Some("héllo".into()), tags: vec![1, 300, 65535] });
        round(Point { x: 0, name: None, tags: vec![] });
        round(Shape::Empty);
        round(Shape::Circle(9, true));
        round(Shape::Rect { w: u64::MAX, h: -0.5 });
        round(BTreeMap::from([(1u32, "a".to_string()), (2, String::new())]));
        round(BTreeSet::from([(3u32, 'x'), (1, 'é')]));
        round(vec![usize::MAX, 0, 127, 128]);
        round(Arc::new([1u8, 2, 3]));
    }

    #[test]
    fn bad_bytes_give_errors() {
        assert!(from_bytes::<String>(&[5, b'a']).is_err());
        assert!(from_bytes::<bool>(&[2]).is_err());
        assert!(from_bytes::<Shape>(&[9]).is_err());
        assert!(from_bytes::<u32>(&[1, 0, 0, 0, 0]).is_err());
        assert!(from_bytes::<Vec<u8>>(&[0xff, 0xff, 0xff, 0xff, 0x0f]).is_err());
        assert!(from_bytes::<u64>(&[0xff; 11][..8]).is_ok());
        let mut input = Reader::new(&[0xff; 11]);
        assert!(input.varint().is_err());
    }
}
