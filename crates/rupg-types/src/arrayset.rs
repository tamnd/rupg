//! The assignment to an element or a slice of an array, as `a[2] := 5` and `a[1:2] := ARRAY[7, 8]` make it.
//!
//! A port of `array_set_element` and `array_set_slice` in `src/backend/utils/adt/arrayfuncs.c`. The caller gives the empty array for a null array, as `array_subscript_assign` of `arraysubs.c` does. An array of fixed length, such as `name`, is not supported here.

use rupg_common::SqlState;

use crate::array::{Array, ArrayDim, MAX_ARRAY_SIZE, MAXDIM, item_count};
use crate::error::TypeError;

fn subscript_error(message: &str) -> TypeError {
    TypeError::new(SqlState::ARRAY_SUBSCRIPT_ERROR, message.to_string())
}

fn too_large() -> TypeError {
    TypeError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("array size exceeds the maximum allowed ({MAX_ARRAY_SIZE})"),
    )
}

/// `ArrayCheckBounds`: the upper bound of each dimension must be in the range of `integer`.
fn check_bounds(dims: &[ArrayDim]) -> Result<(), TypeError> {
    for dim in dims {
        if dim.len.checked_add(dim.lower).is_none() {
            return Err(TypeError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("array lower bound is too large: {}", dim.lower),
            ));
        }
    }
    Ok(())
}

/// The dimension from `lower` to `end`, where `end` is after the last element. A dimension with more elements than an array can have is an error.
fn dim_between(lower: i64, end: i64) -> Result<ArrayDim, TypeError> {
    let len = end - lower;
    if len > MAX_ARRAY_SIZE as i64 {
        return Err(too_large());
    }
    let (Ok(len), Ok(lower)) = (i32::try_from(len), i32::try_from(lower)) else {
        return Err(too_large());
    };
    Ok(ArrayDim { len, lower })
}

/// The offset in the storage of the element at the positions `at`, which count from 0 in each dimension.
fn offset(dims: &[ArrayDim], at: &[usize]) -> usize {
    dims.iter().zip(at).fold(0, |offset, (dim, &i)| offset * dim.len as usize + i)
}

impl<T> Array<T> {
    /// `array_set_element`: the array with `value` at the subscripts. The empty array becomes an array with one element at the subscripts. A one-dimensional array grows with null elements to reach a subscript outside it, and another array gives an error for a subscript outside it.
    ///
    /// # Errors
    ///
    /// `2202E` for a number of subscripts that is not the number of dimensions and for a subscript outside an array of more than one dimension, and `54000` for an array that becomes too large.
    pub fn set_element(self, subscripts: &[i32], value: Option<T>) -> Result<Array<T>, TypeError> {
        let n = subscripts.len();
        if n == 0 || n > MAXDIM {
            return Err(subscript_error("wrong number of array subscripts"));
        }
        let Array { mut dims, mut values } = self;
        if dims.is_empty() {
            // `construct_md_array`: one element, with the subscripts as the lower bounds.
            let dims: Vec<_> = subscripts.iter().map(|&lower| ArrayDim { len: 1, lower }).collect();
            check_bounds(&dims)?;
            return Ok(Array { dims, values: vec![value] });
        }
        if dims.len() != n {
            return Err(subscript_error("wrong number of array subscripts"));
        }
        if n == 1 {
            let dim = dims[0];
            let (lower, end) = (i64::from(dim.lower), i64::from(dim.lower) + i64::from(dim.len));
            let at = i64::from(subscripts[0]);
            let grown = dim_between(lower.min(at), end.max(at + 1))?;
            check_bounds(&[grown])?;
            let before = (lower - i64::from(grown.lower)) as usize;
            let after = grown.len as usize - before - dim.len as usize;
            values.splice(0..0, std::iter::repeat_with(|| None).take(before));
            values.extend(std::iter::repeat_with(|| None).take(after));
            dims[0] = grown;
            values[(at - i64::from(grown.lower)) as usize] = value;
            return Ok(Array { dims, values });
        }
        let mut at = Vec::with_capacity(n);
        for (dim, &i) in dims.iter().zip(subscripts) {
            let step = i64::from(i) - i64::from(dim.lower);
            if step < 0 || step >= i64::from(dim.len) {
                return Err(subscript_error("array subscript out of range"));
            }
            at.push(step as usize);
        }
        let place = offset(&dims, &at);
        values[place] = value;
        Ok(Array { dims, values })
    }

    /// `array_set_slice`: the array with the elements of `source` in the slice between the bounds, in the order of storage. A bound that the query does not give is the bound of the array, and a dimension with no subscripts is the full dimension. The empty array becomes an array with the dimensions of the slice, whose bounds must all be given. A one-dimensional array grows with null elements to reach the slice. The elements of `source` after the elements that the slice needs do not count.
    ///
    /// # Errors
    ///
    /// `2202E` for more subscripts than dimensions, for a missing bound of a slice of the empty array, for an upper bound under its lower bound, for a bound outside an array of more than one dimension and for a source with too few elements, and `54000` for an array that becomes too large.
    pub fn set_slice(
        self,
        lower: &[Option<i32>],
        upper: &[Option<i32>],
        source: Array<T>,
    ) -> Result<Array<T>, TypeError> {
        let n = upper.len();
        let Array { mut dims, mut values } = self;
        let source = source.values;
        if dims.is_empty() {
            let mut dims = Vec::with_capacity(n);
            for (lo, up) in lower.iter().zip(upper) {
                let (Some(lo), Some(up)) = (lo, up) else {
                    let mut error =
                        subscript_error("array slice subscript must provide both boundaries");
                    error.detail = Some(
                        "When assigning to a slice of an empty array value, slice boundaries must be fully specified."
                            .to_string(),
                    );
                    return Err(error);
                };
                let len = i64::from(*up) - i64::from(*lo) + 1;
                let len = i32::try_from(len).map_err(|_| too_large())?;
                dims.push(ArrayDim { len, lower: *lo });
            }
            let count = item_count(&dims)?;
            if source.len() < count {
                return Err(subscript_error("source array too small"));
            }
            check_bounds(&dims)?;
            if count == 0 {
                return Ok(Array::empty());
            }
            return Ok(Array { dims, values: source.into_iter().take(count).collect() });
        }
        if dims.len() < n {
            return Err(subscript_error("wrong number of array subscripts"));
        }
        let mut ranges = Vec::with_capacity(dims.len());
        for (i, dim) in dims.iter().enumerate() {
            let (first, end) = (i64::from(dim.lower), i64::from(dim.lower) + i64::from(dim.len));
            let lo = lower.get(i).copied().flatten().map_or(first, i64::from);
            let up = upper.get(i).copied().flatten().map_or(end - 1, i64::from);
            if lo > up {
                return Err(subscript_error("upper bound cannot be less than lower bound"));
            }
            if dims.len() > 1 && (lo < first || up >= end) {
                return Err(subscript_error("array subscript out of range"));
            }
            ranges.push((lo, up));
        }
        if dims.len() == 1 {
            let dim = dims[0];
            let (lo, up) = ranges[0];
            let (lower, end) = (i64::from(dim.lower), i64::from(dim.lower) + i64::from(dim.len));
            let grown = dim_between(lower.min(lo), end.max(up + 1))?;
            check_bounds(&[grown])?;
            let span = (up - lo + 1) as usize;
            if source.len() < span {
                return Err(subscript_error("source array too small"));
            }
            let before = (lower - i64::from(grown.lower)) as usize;
            let after = grown.len as usize - before - dim.len as usize;
            values.splice(0..0, std::iter::repeat_with(|| None).take(before));
            values.extend(std::iter::repeat_with(|| None).take(after));
            dims[0] = grown;
            let start = (lo - i64::from(grown.lower)) as usize;
            values.splice(start..start + span, source.into_iter().take(span));
            return Ok(Array { dims, values });
        }
        let span: usize = ranges.iter().map(|&(lo, up)| (up - lo + 1) as usize).product();
        if source.len() < span {
            return Err(subscript_error("source array too small"));
        }
        // The positions of the slice from 0 in each dimension, in the order of storage.
        let mut at: Vec<usize> = ranges
            .iter()
            .zip(&dims)
            .map(|(&(lo, _), dim)| (lo - i64::from(dim.lower)) as usize)
            .collect();
        for item in source.into_iter().take(span) {
            let place = offset(&dims, &at);
            values[place] = item;
            for level in (0..at.len()).rev() {
                let (lo, up) = ranges[level];
                let first = (lo - i64::from(dims[level].lower)) as usize;
                if at[level] < (up - i64::from(dims[level].lower)) as usize {
                    at[level] += 1;
                    break;
                }
                at[level] = first;
            }
        }
        Ok(Array { dims, values })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(values: &[i32]) -> Array<i32> {
        Array::one(values.iter().copied().map(Some).collect())
    }

    fn grid(rows: &[&[i32]]) -> Array<i32> {
        let len = rows[0].len() as i32;
        let dims = vec![ArrayDim { len: rows.len() as i32, lower: 1 }, ArrayDim { len, lower: 1 }];
        Array { dims, values: rows.iter().flat_map(|r| r.iter().copied().map(Some)).collect() }
    }

    fn values(array: &Array<i32>) -> Vec<Option<i32>> {
        array.values.clone()
    }

    #[test]
    fn element() {
        let a = one(&[1, 2, 3]).set_element(&[5], Some(5)).unwrap();
        assert_eq!(a.dims, vec![ArrayDim { len: 5, lower: 1 }]);
        assert_eq!(values(&a), vec![Some(1), Some(2), Some(3), None, Some(5)]);
        let a = one(&[1]).set_element(&[-1], Some(9)).unwrap();
        assert_eq!(a.dims, vec![ArrayDim { len: 3, lower: -1 }]);
        assert_eq!(values(&a), vec![Some(9), None, Some(1)]);
        let a = Array::empty().set_element(&[2, 3], Some(7)).unwrap();
        let dims = vec![ArrayDim { len: 1, lower: 2 }, ArrayDim { len: 1, lower: 3 }];
        assert_eq!(a.dims, dims);
        let a = grid(&[&[1, 2], &[3, 4]]).set_element(&[2, 1], Some(30)).unwrap();
        assert_eq!(values(&a), vec![Some(1), Some(2), Some(30), Some(4)]);
        let out = grid(&[&[1, 2], &[3, 4]]).set_element(&[3, 1], Some(5)).unwrap_err();
        assert_eq!(out.message, "array subscript out of range");
        let out = one(&[1, 2]).set_element(&[1, 1], Some(5)).unwrap_err();
        assert_eq!(out.message, "wrong number of array subscripts");
        let out = one(&[1, 2, 3]).set_element(&[i32::MAX], Some(1)).unwrap_err();
        assert_eq!(out.sqlstate, SqlState::PROGRAM_LIMIT_EXCEEDED);
        let out = Array::empty().set_element(&[i32::MAX], Some(1)).unwrap_err();
        assert_eq!(out.message, "array lower bound is too large: 2147483647");
    }

    #[test]
    fn slice() {
        let a = one(&[1, 2, 3]).set_slice(&[Some(5)], &[Some(6)], one(&[50, 60])).unwrap();
        assert_eq!(values(&a), vec![Some(1), Some(2), Some(3), None, Some(50), Some(60)]);
        let a = one(&[1, 2, 3]).set_slice(&[None], &[Some(2)], one(&[7, 8, 9])).unwrap();
        assert_eq!(values(&a), vec![Some(7), Some(8), Some(3)]);
        let a = Array::empty().set_slice(&[Some(2)], &[Some(3)], one(&[1, 2])).unwrap();
        assert_eq!(a.dims, vec![ArrayDim { len: 2, lower: 2 }]);
        let a = Array::empty().set_slice(&[Some(3)], &[Some(2)], one(&[1, 2])).unwrap();
        assert!(a.dims.is_empty());
        let out = Array::empty().set_slice(&[None], &[Some(3)], one(&[1])).unwrap_err();
        assert_eq!(out.message, "array slice subscript must provide both boundaries");
        let out = one(&[1, 2, 3]).set_slice(&[Some(1)], &[Some(3)], one(&[1, 2])).unwrap_err();
        assert_eq!(out.message, "source array too small");
        let out = one(&[1, 2, 3]).set_slice(&[Some(3)], &[Some(1)], one(&[1, 2])).unwrap_err();
        assert_eq!(out.message, "upper bound cannot be less than lower bound");
        let a = grid(&[&[1, 2, 3], &[4, 5, 6]]);
        let a = a.set_slice(&[Some(1), Some(2)], &[Some(2), Some(3)], one(&[20, 30, 50, 60]));
        let a = a.unwrap();
        let expect = [1, 20, 30, 4, 50, 60].map(Some).to_vec();
        assert_eq!(values(&a), expect);
        let a = a.set_slice(&[Some(2)], &[Some(2)], one(&[-1, -2, -3])).unwrap();
        assert_eq!(values(&a), [1, 20, 30, -1, -2, -3].map(Some).to_vec());
        let out = a.set_slice(&[Some(1), Some(1)], &[Some(3), Some(1)], one(&[1, 2, 3]));
        assert_eq!(out.unwrap_err().message, "array subscript out of range");
    }
}
