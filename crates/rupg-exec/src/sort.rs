//! The sort of the rows for `ORDER BY`, `DISTINCT` and `DISTINCT ON`. `qsort` is `pg_qsort` of `sort_template.h` in `rupg-common`, and [`bounded`] is the bounded heap of `tuplesort.c`. The engine does the same comparisons as PostgreSQL, so the rows that are equal on all keys come in the same order.

use std::cmp::Ordering;

use rupg_analyze::SortGroup;
use rupg_common::Result;
use rupg_func::{Call, Kernel, Session};
use rupg_types::{Value, oid};

use crate::kernel;

/// `qsort` of `rupg-common` with the errors of the engine.
pub(crate) fn qsort<T, F>(data: &mut [T], cmp: &mut F) -> Result<()>
where
    F: FnMut(&T, &T) -> Result<Ordering>,
{
    rupg_common::qsort(data, cmp)
}

/// A key of a sort with the kernels of its functions.
struct Key {
    column: usize,
    ty: u32,
    sort: Option<Kernel>,
    equal: Option<Kernel>,
    nulls_first: bool,
    /// For a sort function that is `<` or `>` of an integer, date, time or text type: true for `>`. The keys then compare the integers or the bytes with no call.
    descending: Option<bool>,
}

/// For the function `<` or `>` of `smallint`, `integer`, `bigint`, `date`, `time`, `timestamp`, `timestamptz` or `text` with itself: true for `>`. The order of these types is the order of their integers, also for the infinite values, and the order of the bytes for `text`, as the comparison of text does here.
fn native(func: u32) -> Option<bool> {
    let name = rupg_pgcatalog::builtin::proc_by_oid(func)?.name;
    match name {
        "int2lt" | "int4lt" | "int8lt" | "date_lt" | "time_lt" | "timestamp_lt"
        | "timestamptz_lt" | "text_lt" => Some(false),
        "int2gt" | "int4gt" | "int8gt" | "date_gt" | "time_gt" | "timestamp_gt"
        | "timestamptz_gt" | "text_gt" => Some(true),
        _ => None,
    }
}

/// The integer of a value of an integer, date or time type.
fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Int2(v) => Some(i64::from(*v)),
        Value::Int4(v) | Value::Date(v) => Some(i64::from(*v)),
        Value::Int8(v) | Value::Time(v) | Value::Timestamp(v) | Value::TimestampTz(v) => Some(*v),
        _ => None,
    }
}

/// The keys of a sort or of a comparison of rows.
pub(crate) struct Keys<'a> {
    keys: Vec<Key>,
    session: &'a dyn Session,
}

impl<'a> Keys<'a> {
    /// The keys of a list of `ORDER BY` or `DISTINCT`. `types` gives the type of each column.
    pub(crate) fn new(list: &[SortGroup], types: &[u32], session: &'a dyn Session) -> Result<Self> {
        let find = |func: u32| if func == 0 { Ok(None) } else { kernel(func, None).map(Some) };
        let keys = list
            .iter()
            .map(|g| {
                Ok(Key {
                    column: g.target,
                    ty: types[g.target],
                    sort: find(g.sort)?,
                    equal: find(g.equal)?,
                    nulls_first: g.nulls_first,
                    descending: native(g.sort),
                })
            })
            .collect::<Result<_>>()?;
        Ok(Keys { keys, session })
    }

    fn test(&self, kernel: Kernel, ty: u32, a: &Value, b: &Value) -> Result<bool> {
        let types = [ty, ty];
        let call = Call {
            session: self.session,
            args: &types,
            ret: oid::BOOL,
            variadic: false,
            want: None,
        };
        Ok(kernel(&call, &[a.clone(), b.clone()])? == Value::Bool(true))
    }

    /// The order of two rows, as `comparetup_heap` gives it. A key with no sort function does not count.
    pub(crate) fn compare(&self, a: &[Value], b: &[Value]) -> Result<Ordering> {
        for key in &self.keys {
            let Some(sort) = key.sort else { continue };
            let (x, y) = (&a[key.column], &b[key.column]);
            if let Some(descending) = key.descending {
                let order = match (x, y) {
                    (Value::Text(x), Value::Text(y)) => Some(x.as_bytes().cmp(y.as_bytes())),
                    _ => int(x).zip(int(y)).map(|(x, y)| x.cmp(&y)),
                };
                if let Some(order) = order {
                    let order = if descending { order.reverse() } else { order };
                    if order != Ordering::Equal {
                        return Ok(order);
                    }
                    continue;
                }
            }
            let order = match (x.is_null(), y.is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) if key.nulls_first => Ordering::Less,
                (true, false) => Ordering::Greater,
                (false, true) if key.nulls_first => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    if self.test(sort, key.ty, x, y)? {
                        Ordering::Less
                    } else if self.test(sort, key.ty, y, x)? {
                        Ordering::Greater
                    } else {
                        Ordering::Equal
                    }
                }
            };
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(Ordering::Equal)
    }

    /// True when two rows are equal on every key, as `execTuplesMatch` gives it. Two nulls are equal.
    pub(crate) fn equal(&self, a: &[Value], b: &[Value]) -> Result<bool> {
        for key in &self.keys {
            let (x, y) = (&a[key.column], &b[key.column]);
            let same = match (x.is_null(), y.is_null()) {
                (true, true) => true,
                (false, false) => match key.equal {
                    Some(equal) => self.test(equal, key.ty, x, y)?,
                    None => false,
                },
                _ => false,
            };
            if !same {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// `tuplesort_heap_replace_top` with the new item already at the top: moves it down to its place.
fn sift_down<T, F>(heap: &mut [T], cmp: &mut F) -> Result<()>
where
    F: FnMut(&T, &T) -> Result<Ordering>,
{
    let n = heap.len();
    let mut i = 0;
    loop {
        let mut j = 2 * i + 1;
        if j >= n {
            break;
        }
        if j + 1 < n && cmp(&heap[j], &heap[j + 1])?.is_gt() {
            j += 1;
        }
        if cmp(&heap[i], &heap[j])?.is_le() {
            break;
        }
        heap.swap(i, j);
        i = j;
    }
    Ok(())
}

/// The first `bound` items in order, as the bounded heap sort of `tuplesort.c` gives them. The heap keeps the `bound` least items with the order reversed, so that the greatest of them is at the top.
pub(crate) fn bounded<T, F>(items: Vec<T>, bound: usize, cmp: &mut F) -> Result<Vec<T>>
where
    F: FnMut(&T, &T) -> Result<Ordering>,
{
    let mut heap = Bounded::new(bound);
    for item in items {
        heap.push(item, cmp)?;
    }
    heap.finish(cmp)
}

/// The bounded heap of `tuplesort.c` that takes the items one at a time, so that the items that do not go in the heap are not kept.
pub(crate) struct Bounded<T> {
    heap: Vec<T>,
    bound: usize,
}

impl<T> Bounded<T> {
    /// An empty heap for the first `bound` items.
    pub(crate) fn new(bound: usize) -> Bounded<T> {
        Bounded { heap: Vec::with_capacity(bound), bound }
    }

    /// Puts the item in the heap. The result is the item that is not in the heap after the step, if there is one: the item, or the old top.
    pub(crate) fn push<F>(&mut self, item: T, cmp: &mut F) -> Result<Option<T>>
    where
        F: FnMut(&T, &T) -> Result<Ordering>,
    {
        let mut reversed = |a: &T, b: &T| cmp(a, b).map(Ordering::reverse);
        let heap = &mut self.heap;
        if heap.len() < self.bound {
            // `tuplesort_heap_insert`: moves the new item up to its place.
            heap.push(item);
            let mut j = heap.len() - 1;
            while j > 0 {
                let i = (j - 1) >> 1;
                if reversed(&heap[j], &heap[i])?.is_ge() {
                    break;
                }
                heap.swap(i, j);
                j = i;
            }
            Ok(None)
        } else if reversed(&item, &heap[0])?.is_gt() {
            let old = std::mem::replace(&mut heap[0], item);
            sift_down(heap, &mut reversed)?;
            Ok(Some(old))
        } else {
            Ok(Some(item))
        }
    }

    /// True when [`Bounded::push`] keeps the item: the heap is not full, or the item comes before its top.
    pub(crate) fn keeps<F>(&self, item: &T, cmp: &mut F) -> Result<bool>
    where
        F: FnMut(&T, &T) -> Result<Ordering>,
    {
        Ok(self.heap.len() < self.bound || cmp(item, &self.heap[0])?.is_lt())
    }

    /// `sort_bounded_heap`: the items of the heap in order.
    pub(crate) fn finish<F>(mut self, cmp: &mut F) -> Result<Vec<T>>
    where
        F: FnMut(&T, &T) -> Result<Ordering>,
    {
        let mut reversed = |a: &T, b: &T| cmp(a, b).map(Ordering::reverse);
        let heap = &mut self.heap;
        // Takes the top until one item is left.
        let mut sorted = Vec::with_capacity(heap.len());
        while heap.len() > 1 {
            sorted.push(heap.swap_remove(0));
            sift_down(heap, &mut reversed)?;
        }
        sorted.append(heap);
        sorted.reverse();
        Ok(sorted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The order of the keys only, so that a test can see which of two equal items comes first.
    fn by_key(a: &(i32, usize), b: &(i32, usize)) -> Result<Ordering> {
        Ok(a.0.cmp(&b.0))
    }

    /// The order of the indexes after a sort of `n` items with the key `i * mul % m`.
    fn order(n: usize, m: i32, mul: i32, bound: Option<usize>) -> Vec<usize> {
        let mut items: Vec<(i32, usize)> =
            (0..n).map(|i| (i32::try_from(i).unwrap() * mul % m, i)).collect();
        if let Some(bound) = bound {
            items = bounded(items, bound, &mut by_key).unwrap();
        } else {
            qsort(&mut items, &mut by_key).unwrap();
        }
        items.iter().map(|i| i.1).collect()
    }

    #[test]
    fn same_order_as_postgres() {
        // The orders come from the qsort of sort_template.h and the heap functions of tuplesort.c, compiled in C.
        assert_eq!(order(10, 2, 1, None), [2, 6, 4, 8, 0, 9, 1, 3, 5, 7]);
        assert_eq!(
            order(50, 3, 1, None),
            [
                36, 18, 3, 33, 21, 45, 30, 24, 9, 27, 0, 42, 12, 48, 39, 15, 6, 49, 1, 4, 7, 10,
                13, 16, 19, 22, 25, 28, 31, 34, 37, 40, 43, 46, 8, 35, 17, 2, 38, 14, 47, 41, 11,
                26, 5, 29, 23, 44, 32, 20
            ]
        );
        assert_eq!(order(30, 3, 1, Some(6)), [9, 12, 3, 0, 6, 15]);
        assert_eq!(order(100, 17, 37, Some(10)), [34, 0, 51, 85, 68, 17, 6, 57, 23, 40]);
    }
}
