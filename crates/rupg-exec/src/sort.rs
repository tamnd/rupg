//! The sort of the rows for `ORDER BY`, `DISTINCT` and `DISTINCT ON`. [`qsort`] is `pg_qsort` of `sort_template.h` and [`bounded`] is the bounded heap of `tuplesort.c`. The engine does the same comparisons as PostgreSQL, so the rows that are equal on all keys come in the same order.

use std::cmp::Ordering;

use rupg_analyze::SortGroup;
use rupg_common::Result;
use rupg_func::{Call, Kernel, Session};
use rupg_types::{Value, oid};

use crate::kernel;

/// A key of a sort with the kernels of its functions.
struct Key {
    column: usize,
    ty: u32,
    sort: Option<Kernel>,
    equal: Option<Kernel>,
    nulls_first: bool,
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
                })
            })
            .collect::<Result<_>>()?;
        Ok(Keys { keys, session })
    }

    fn test(&self, kernel: Kernel, ty: u32, a: &Value, b: &Value) -> Result<bool> {
        let types = [ty, ty];
        let call = Call { session: self.session, args: &types, ret: oid::BOOL, variadic: false };
        Ok(kernel(&call, &[a.clone(), b.clone()])? == Value::Bool(true))
    }

    /// The order of two rows, as `comparetup_heap` gives it. A key with no sort function does not count.
    pub(crate) fn compare(&self, a: &[Value], b: &[Value]) -> Result<Ordering> {
        for key in &self.keys {
            let Some(sort) = key.sort else { continue };
            let (x, y) = (&a[key.column], &b[key.column]);
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

/// `med3`: the index of the median of three items.
fn med3<T, F>(data: &[T], a: usize, b: usize, c: usize, cmp: &mut F) -> Result<usize>
where
    F: FnMut(&T, &T) -> Result<Ordering>,
{
    Ok(if cmp(&data[a], &data[b])?.is_lt() {
        if cmp(&data[b], &data[c])?.is_lt() {
            b
        } else if cmp(&data[a], &data[c])?.is_lt() {
            c
        } else {
            a
        }
    } else if cmp(&data[b], &data[c])?.is_gt() {
        b
    } else if cmp(&data[a], &data[c])?.is_lt() {
        a
    } else {
        c
    })
}

/// `swapn`: swaps `n` items at `a` with `n` items at `b`.
fn swapn<T>(data: &mut [T], a: usize, b: usize, n: usize) {
    for i in 0..n {
        data.swap(a + i, b + i);
    }
}

/// `pg_qsort`: a quicksort with the median of three or of nine, a partition in three parts and an insertion sort for fewer than 7 items. It is not stable.
pub(crate) fn qsort<T, F>(data: &mut [T], cmp: &mut F) -> Result<()>
where
    F: FnMut(&T, &T) -> Result<Ordering>,
{
    let mut a = 0;
    let mut n = data.len();
    loop {
        if n < 7 {
            for pm in a + 1..a + n {
                let mut pl = pm;
                while pl > a && cmp(&data[pl - 1], &data[pl])?.is_gt() {
                    data.swap(pl, pl - 1);
                    pl -= 1;
                }
            }
            return Ok(());
        }
        let mut presorted = true;
        for pm in a + 1..a + n {
            if cmp(&data[pm - 1], &data[pm])?.is_gt() {
                presorted = false;
                break;
            }
        }
        if presorted {
            return Ok(());
        }
        let mut pm = a + n / 2;
        if n > 7 {
            let mut pl = a;
            let mut last = a + n - 1;
            if n > 40 {
                let d = n / 8;
                pl = med3(data, pl, pl + d, pl + 2 * d, cmp)?;
                pm = med3(data, pm - d, pm, pm + d, cmp)?;
                last = med3(data, last - 2 * d, last - d, last, cmp)?;
            }
            pm = med3(data, pl, pm, last, cmp)?;
        }
        data.swap(a, pm);
        let (mut pa, mut pb) = (a + 1, a + 1);
        let (mut pc, mut pd) = (a + n - 1, a + n - 1);
        loop {
            while pb <= pc {
                let r = cmp(&data[pb], &data[a])?;
                if r.is_gt() {
                    break;
                }
                if r.is_eq() {
                    data.swap(pa, pb);
                    pa += 1;
                }
                pb += 1;
            }
            while pb <= pc {
                let r = cmp(&data[pc], &data[a])?;
                if r.is_lt() {
                    break;
                }
                if r.is_eq() {
                    data.swap(pc, pd);
                    pd -= 1;
                }
                pc -= 1;
            }
            if pb > pc {
                break;
            }
            data.swap(pb, pc);
            pb += 1;
            pc -= 1;
        }
        let end = a + n;
        let d1 = (pa - a).min(pb - pa);
        swapn(data, a, pb - d1, d1);
        let d1 = (pd - pc).min(end - pd - 1);
        swapn(data, pb, end - d1, d1);
        let (d1, d2) = (pb - pa, pd - pc);
        if d1 <= d2 {
            qsort(&mut data[a..a + d1], cmp)?;
            a = end - d2;
            n = d2;
        } else {
            qsort(&mut data[end - d2..end], cmp)?;
            n = d1;
        }
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
    let mut reversed = |a: &T, b: &T| cmp(a, b).map(Ordering::reverse);
    let mut heap: Vec<T> = Vec::with_capacity(bound);
    for item in items {
        if heap.len() < bound {
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
        } else if reversed(&item, &heap[0])?.is_gt() {
            heap[0] = item;
            sift_down(&mut heap, &mut reversed)?;
        }
    }
    // `sort_bounded_heap`: takes the top until one item is left.
    let mut sorted = Vec::with_capacity(heap.len());
    while heap.len() > 1 {
        sorted.push(heap.swap_remove(0));
        sift_down(&mut heap, &mut reversed)?;
    }
    sorted.extend(heap);
    sorted.reverse();
    Ok(sorted)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The order of the keys only, so that a test can see which of two equal items comes first.
    fn by_key(a: &(i32, usize), b: &(i32, usize)) -> Result<Ordering> {
        Ok(a.0.cmp(&b.0))
    }

    #[test]
    fn qsort_sorts() {
        for n in [0, 1, 5, 7, 8, 40, 41, 200, 1000] {
            let mut items: Vec<(i32, usize)> =
                (0..n).map(|i| (i32::try_from(i).unwrap() * 7919 % 13, i)).collect();
            qsort(&mut items, &mut by_key).unwrap();
            assert!(items.windows(2).all(|w| w[0].0 <= w[1].0), "n = {n}");
            let mut seen: Vec<usize> = items.iter().map(|i| i.1).collect();
            seen.sort_unstable();
            assert_eq!(seen, (0..n).collect::<Vec<_>>());
        }
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
