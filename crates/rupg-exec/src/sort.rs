//! The sort of the rows for `ORDER BY`, `DISTINCT` and `DISTINCT ON`. `qsort` is `pg_qsort` of `sort_template.h` in `rupg-common`, and [`bounded`] is the bounded heap of `tuplesort.c`. The engine does the same comparisons as PostgreSQL, so the rows that are equal on all keys come in the same order.

use std::cmp::Ordering;

use rupg_analyze::SortGroup;
use rupg_common::Result;
use rupg_func::{Call, Collator, Kernel, Session};
use rupg_types::{Value, oid};

use crate::kernel;

/// `qsort` of `rupg-common` with the errors of the engine.
pub(crate) fn qsort<T, F>(data: &mut [T], cmp: &mut F) -> Result<()>
where
    F: FnMut(&T, &T) -> Result<Ordering>,
{
    rupg_common::qsort(data, cmp)
}

/// `QSORT_THRESHOLD` of `tuplesort.c`: a sort of fewer items than this is a `qsort`.
const QSORT_THRESHOLD: usize = 40;

/// How `normalize_datum` of `tuplesort.c` makes an unsigned integer from the value of the first key, for the comparators `ssup_datum_int32_cmp` and `ssup_datum_signed_cmp`.
#[derive(Clone, Copy)]
pub(crate) enum Radix {
    /// `integer` and `date`.
    Int32,
    /// `bigint`, `timestamp` and `timestamptz`.
    Signed,
}

impl Radix {
    /// `normalize_datum`: an unsigned integer whose order is the order of the value, reversed when `reverse` is true. `None` when the value is not an integer of the kind.
    pub(crate) fn normalize(self, value: i64, reverse: bool) -> Option<u64> {
        let norm = match self {
            Radix::Int32 => {
                u64::from(i32::try_from(value).ok()?.cast_unsigned().wrapping_add(1 << 31))
            }
            Radix::Signed => value.cast_unsigned().wrapping_add(1 << 63),
        };
        Some(if reverse { !norm } else { norm })
    }
}

/// For the function `<` or `>` whose sort support has an integer comparator: the kind of its integers, and true for `>`.
fn radix(func: u32) -> Option<(Radix, bool)> {
    match rupg_pgcatalog::builtin::proc_by_oid(func)?.name {
        "int4lt" | "date_lt" => Some((Radix::Int32, false)),
        "int4gt" | "date_gt" => Some((Radix::Int32, true)),
        "int8lt" | "timestamp_lt" | "timestamptz_lt" => Some((Radix::Signed, false)),
        "int8gt" | "timestamp_gt" | "timestamptz_gt" => Some((Radix::Signed, true)),
        _ => None,
    }
}

/// An item of [`memtuples`]: the normalized value of its first key, true for a null, the byte of the current level, and the place of the item in the data.
#[derive(Clone, Copy)]
struct Tuple {
    norm: u64,
    null: bool,
    byte: u8,
    at: usize,
}

/// `tuplesort_sort_memtuples` of `tuplesort.c` for a first key whose comparator is an integer comparator. `datums` gives the normalized value of the first key of each item, `None` for a null. For 40 or more items, the sort is the radix sort of PostgreSQL 19, so the items that are equal on all keys come in the same order. When `ties` is false, `cmp` compares only the first key, and the sort does not sort the items with equal first keys again. Then `cmp` gives no order to them, and a `qsort` keeps their order.
pub(crate) fn memtuples<T, F>(
    data: &mut [T],
    datums: &[Option<u64>],
    nulls_first: bool,
    ties: bool,
    cmp: &mut F,
) -> Result<()>
where
    F: FnMut(&T, &T) -> Result<Ordering>,
{
    if data.len() < QSORT_THRESHOLD {
        return qsort(data, cmp);
    }
    let mut tuples: Vec<Tuple> = datums
        .iter()
        .enumerate()
        .map(|(at, d)| Tuple { norm: d.unwrap_or(0), null: d.is_none(), byte: 0, at })
        .collect();
    {
        let items = &*data;
        let mut by = |a: &Tuple, b: &Tuple| cmp(&items[a.at], &items[b.at]);
        radix_sort_tuple(&mut tuples, nulls_first, ties, &mut by)?;
    }
    // Puts each item at its place: the item at place `i` comes from `tuples[i].at`. Each cycle of the permutation moves with swaps.
    let mut from: Vec<usize> = tuples.iter().map(|t| t.at).collect();
    for i in 0..from.len() {
        let mut cur = i;
        while from[cur] != i {
            let next = from[cur];
            data.swap(cur, next);
            from[cur] = cur;
            cur = next;
        }
        from[cur] = cur;
    }
    Ok(())
}

/// `radix_sort_tuple`: puts the nulls of the first key before or after the other items with the branchless partition of Lomuto, then sorts both parts.
fn radix_sort_tuple<F>(data: &mut [Tuple], nulls_first: bool, ties: bool, cmp: &mut F) -> Result<()>
where
    F: FnMut(&Tuple, &Tuple) -> Result<Ordering>,
{
    let n = data.len();
    let mut d1 = 0;
    while d1 < n && data[d1].null == nulls_first {
        d1 += 1;
    }
    if d1 + 1 < n {
        let (mut i, mut j) = (d1, d1);
        let gap = data[d1];
        while j < n - 1 {
            data[j] = data[i];
            j += 1;
            data[i] = data[j];
            i += usize::from(data[i].null == nulls_first);
        }
        data[j] = data[i];
        data[i] = gap;
        i += usize::from(data[i].null == nulls_first);
        d1 = i;
    }
    let (left, right) = data.split_at_mut(d1);
    let (nulls, values) = if nulls_first { (left, right) } else { (right, left) };
    if ties && nulls.len() > 1 {
        qsort(nulls, cmp)?;
    }
    if values.len() < QSORT_THRESHOLD {
        return qsort(values, cmp);
    }
    for i in 1..values.len() {
        if cmp(&values[i - 1], &values[i])?.is_gt() {
            return radix_sort_recursive(values, 0, ties, cmp);
        }
    }
    Ok(())
}

/// `radix_sort_recursive`: the radix sort of `ska_byte_sort` on the byte `level` of the normalized values, 0 for the most significant byte, with a `qsort` for a part of fewer than 40 items.
fn radix_sort_recursive<F>(data: &mut [Tuple], level: u32, ties: bool, cmp: &mut F) -> Result<()>
where
    F: FnMut(&Tuple, &Tuple) -> Result<Ordering>,
{
    let mut count = [0usize; 256];
    let reference = data[0].norm;
    let mut common = 0u64;
    let shift = (7 - level) * 8;
    for t in data.iter_mut() {
        common |= reference ^ t.norm;
        t.byte = ((t.norm >> shift) & 0xFF) as u8;
        count[usize::from(t.byte)] += 1;
    }
    let (mut offset, mut next) = ([0usize; 256], [0usize; 256]);
    let mut remaining = [0u8; 256];
    let (mut total, mut parts) = (0, 0);
    for (byte, &c) in (0u8..=255).zip(&count) {
        let b = usize::from(byte);
        if c != 0 {
            offset[b] = total;
            total += c;
            remaining[parts] = byte;
            parts += 1;
        }
        next[b] = total;
    }
    // Each step sends the item to the place of its part and goes on to the next item, so the loop can need more than one pass.
    let mut left = parts;
    while left > 1 {
        left = parts;
        for &idx in &remaining[..parts] {
            let idx = usize::from(idx);
            for st in offset[idx]..next[idx] {
                let b = usize::from(data[st].byte);
                let to = offset[b];
                offset[b] += 1;
                data.swap(st, to);
            }
            if offset[idx] == next[idx] {
                left -= 1;
            }
        }
    }
    // With one part, the next level is the first byte where two values are not equal.
    let next_level = if parts > 1 {
        level + 1
    } else if common == 0 {
        8
    } else {
        7 - (63 - common.leading_zeros()) / 8
    };
    let mut start = 0;
    for &idx in &remaining[..parts] {
        let end = next[usize::from(idx)];
        let part = &mut data[start..end];
        if part.len() > 1 {
            if next_level < 8 {
                if part.len() < QSORT_THRESHOLD {
                    qsort(part, cmp)?;
                } else {
                    radix_sort_recursive(part, next_level, ties, cmp)?;
                }
            } else if ties {
                qsort(part, cmp)?;
            }
        }
        start = end;
    }
    Ok(())
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
    /// For `<` or `>` of `text` in a collation of ICU: the collator, and true for `>`.
    collated: Option<(Collator, bool)>,
    /// The collation of ICU of the key, which the calls of its functions get, or 0.
    collation: u32,
    /// For a sort function that is `<` or `>` of `character` in a collation that orders the bytes: true for `>`. The keys then compare the bytes with no trailing spaces with no call.
    padded: Option<bool>,
    /// For a sort function whose sort support has an integer comparator: the kind of its integers, and true for `>`.
    radix: Option<(Radix, bool)>,
    /// When the equality function is `=` of an integer, date, time, `text` or `character` type, in a collation whose equality is the equality of the bytes: true for `character`. The keys then compare the integers or the bytes with no call.
    same: Option<bool>,
}

/// For the function `<` or `>` of `character` with itself: true for `>`. In a collation that orders the bytes, the order is the order of the bytes with no trailing spaces, as `bpcharcmp` gives it.
fn padded(func: u32) -> Option<bool> {
    match rupg_pgcatalog::builtin::proc_by_oid(func)?.name {
        "bpcharlt" => Some(false),
        "bpchargt" => Some(true),
        _ => None,
    }
}

/// For the function `=` of `smallint`, `integer`, `bigint`, `date`, `time`, `timestamp`, `timestamptz`, `text` or `character` with itself: true for `character`. In a collation whose equality is the equality of the bytes, two values of these types are equal when their integers or their bytes are equal, with no trailing spaces for `character`.
fn same(func: u32) -> Option<bool> {
    match rupg_pgcatalog::builtin::proc_by_oid(func)?.name {
        "int2eq" | "int4eq" | "int8eq" | "date_eq" | "time_eq" | "timestamp_eq"
        | "timestamptz_eq" | "texteq" => Some(false),
        "bpchareq" => Some(true),
        _ => None,
    }
}

/// True for a collation whose order and equality are the order and the equality of the bytes: no collation, the default collation, `C` and `POSIX`.
fn bytewise(collation: u32) -> bool {
    matches!(collation, 0 | 100 | 950 | 951)
}

/// For the function `<` or `>` of `smallint`, `integer`, `bigint`, `date`, `time`, `timestamp`, `timestamptz`, `numeric` or `text` with itself: true for `>`. The order of these types is the order of their integers, also for the infinite values, and the order of the bytes for `text`, as the comparison of text does here. A `numeric` compares as an integer only in a column of integers, and a value of `numeric` calls the function.
fn native(func: u32) -> Option<bool> {
    let name = rupg_pgcatalog::builtin::proc_by_oid(func)?.name;
    match name {
        "int2lt" | "int4lt" | "int8lt" | "date_lt" | "time_lt" | "timestamp_lt"
        | "timestamptz_lt" | "numeric_lt" | "text_lt" => Some(false),
        "int2gt" | "int4gt" | "int8gt" | "date_gt" | "time_gt" | "timestamp_gt"
        | "timestamptz_gt" | "numeric_gt" | "text_gt" => Some(true),
        _ => None,
    }
}

/// For a key whose equality function compares with no call: true when the two values are equal, with no trailing spaces when `trim` is true, or `None` when a value is not an integer or text.
fn native_equal(trim: bool, x: &Value, y: &Value) -> Option<bool> {
    match (x, y) {
        (Value::Text(x), Value::Text(y)) if trim => {
            Some(x.trim_end_matches(' ') == y.trim_end_matches(' '))
        }
        (Value::Text(x), Value::Text(y)) => Some(x == y),
        _ => int(x).zip(int(y)).map(|(x, y)| x == y),
    }
}

/// The integer of a value of an integer, date or time type.
pub(crate) fn int(v: &Value) -> Option<i64> {
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
                let ty = types[g.target];
                let text = matches!(ty, oid::TEXT | oid::VARCHAR);
                let collator = rupg_func::collator(session, g.collation);
                let bytes = collator.is_none() && bytewise(g.collation);
                let collated =
                    native(g.sort).filter(|_| text).and_then(|desc| Some((collator?, desc)));
                Ok(Key {
                    column: g.target,
                    ty,
                    sort: find(g.sort)?,
                    equal: find(g.equal)?,
                    nulls_first: g.nulls_first,
                    descending: if collator.is_some() { None } else { native(g.sort) },
                    collated,
                    collation: if collator.is_some() { g.collation } else { 0 },
                    padded: padded(g.sort).filter(|_| bytes),
                    same: same(g.equal).filter(|_| bytes),
                    radix: radix(g.sort),
                })
            })
            .collect::<Result<_>>()?;
        Ok(Keys { keys, session })
    }

    /// For one key whose sort function compares integers with no call: true for a descending order.
    pub(crate) fn one_native(&self) -> Option<bool> {
        match self.keys.as_slice() {
            [key] if key.sort.is_some() => key.descending,
            _ => None,
        }
    }

    /// For one key whose sort function has an integer comparator: the kind of its integers, which [`memtuples`] needs.
    pub(crate) fn one_radix(&self) -> Option<Radix> {
        match self.keys.as_slice() {
            [key] if key.sort.is_some() => key.radix.map(|(radix, _)| radix),
            _ => None,
        }
    }

    /// For the first key with a sort function, when the function compares integers or bytes with no call: its column, true for a descending order, and true for nulls first.
    pub(crate) fn lead(&self) -> Option<(usize, bool, bool)> {
        let key = self.keys.iter().find(|key| key.sort.is_some())?;
        Some((key.column, key.descending?, key.nulls_first))
    }

    /// For keys whose sort functions all compare integers or bytes with no call: the column, true for a descending order, and true for nulls first, of each key with a sort function.
    pub(crate) fn natives(&self) -> Option<Vec<(usize, bool, bool)>> {
        self.keys
            .iter()
            .filter(|key| key.sort.is_some())
            .map(|key| Some((key.column, key.descending?, key.nulls_first)))
            .collect()
    }

    /// Sorts the items on the rows that `row` gives, as the sort of `tuplesort.c` in memory with no bound does: the radix sort of [`memtuples`] when the first key has an integer comparator, else `qsort`.
    pub(crate) fn sort<T>(&self, data: &mut [T], row: impl Fn(&T) -> &[Value]) -> Result<()> {
        let mut cmp = |a: &T, b: &T| self.compare(row(a), row(b));
        let mut sorting = self.keys.iter().filter(|key| key.sort.is_some());
        let lead = sorting.next();
        let ties = sorting.next().is_some();
        if let Some(key) = lead
            && let Some((kind, reverse)) = key.radix
            && data.len() >= QSORT_THRESHOLD
            && let Some(datums) = data
                .iter()
                .map(|t| match &row(t)[key.column] {
                    Value::Null => Some(None),
                    v => int(v).and_then(|v| kind.normalize(v, reverse)).map(Some),
                })
                .collect::<Option<Vec<Option<u64>>>>()
        {
            return memtuples(data, &datums, key.nulls_first, ties, &mut cmp);
        }
        qsort(data, &mut cmp)
    }

    fn test(&self, kernel: Kernel, key: &Key, a: &Value, b: &Value) -> Result<bool> {
        let types = [key.ty, key.ty];
        let call = Call {
            session: self.session,
            args: &types,
            ret: oid::BOOL,
            variadic: false,
            want: None,
            collation: key.collation,
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
            if let (Some(descending), Value::Text(x), Value::Text(y)) = (key.padded, x, y) {
                let order =
                    x.trim_end_matches(' ').as_bytes().cmp(y.trim_end_matches(' ').as_bytes());
                let order = if descending { order.reverse() } else { order };
                if order != Ordering::Equal {
                    return Ok(order);
                }
                continue;
            }
            let order = match (x.is_null(), y.is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) if key.nulls_first => Ordering::Less,
                (true, false) => Ordering::Greater,
                (false, true) if key.nulls_first => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    if let (Some((collator, desc)), Value::Text(x), Value::Text(y)) =
                        (key.collated, x, y)
                    {
                        let order = collator.compare(x, y);
                        if desc { order.reverse() } else { order }
                    } else if self.test(sort, key, x, y)? {
                        Ordering::Less
                    } else if self.test(sort, key, y, x)? {
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
                    Some(equal) => match key.same.and_then(|trim| native_equal(trim, x, y)) {
                        Some(same) => same,
                        None => self.test(equal, key, x, y)?,
                    },
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

    /// The top of a full heap: an item goes in only when it comes before the top.
    pub(crate) fn top(&self) -> Option<&T> {
        self.heap.first().filter(|_| self.heap.len() >= self.bound)
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

    /// The order of the indexes after [`memtuples`] of `n` items, with the first key `first(i)` of the kind `kind`, descending when `reverse` is true, and the second key `second(i)` descending when there is one.
    fn radix_order(
        n: i64,
        first: impl Fn(i64) -> Option<i64>,
        (kind, reverse, nulls_first): (Radix, bool, bool),
        second: Option<fn(i64) -> i64>,
    ) -> Vec<i64> {
        let mut items: Vec<(Option<i64>, i64, i64)> =
            (0..n).map(|i| (first(i), second.map_or(0, |f| f(i)), i)).collect();
        let datums: Vec<Option<u64>> =
            items.iter().map(|t| t.0.map(|v| kind.normalize(v, reverse).unwrap())).collect();
        let mut cmp = |a: &(Option<i64>, i64, i64), b: &(Option<i64>, i64, i64)| {
            let order = match (a.0, b.0) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) if nulls_first => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) if nulls_first => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(x), Some(y)) if reverse => y.cmp(&x),
                (Some(x), Some(y)) => x.cmp(&y),
            };
            Ok(order.then(b.1.cmp(&a.1)))
        };
        memtuples(&mut items, &datums, nulls_first, second.is_some(), &mut cmp).unwrap();
        items.iter().map(|t| t.2).collect()
    }

    #[test]
    fn same_order_as_radix_sort_of_postgres() {
        // The orders come from PostgreSQL 19, as `SELECT i FROM generate_series(0, n - 1) i ORDER BY ...` gives them.
        let int4 = (Radix::Int32, false, false);
        assert_eq!(
            radix_order(100, |i| Some(i % 7), int4, None),
            [
                0, 7, 14, 21, 28, 35, 42, 49, 56, 70, 84, 98, 77, 91, 63, 1, 8, 22, 29, 36, 43, 50,
                57, 64, 71, 85, 99, 78, 92, 15, 2, 9, 23, 37, 51, 65, 30, 58, 86, 16, 72, 79, 93,
                44, 3, 10, 17, 24, 38, 52, 66, 80, 73, 87, 31, 45, 59, 94, 4, 11, 18, 25, 39, 53,
                67, 81, 95, 60, 88, 46, 74, 32, 5, 12, 19, 26, 33, 40, 54, 68, 82, 96, 89, 47, 61,
                75, 6, 13, 20, 27, 34, 41, 55, 69, 83, 97, 48, 62, 76, 90
            ]
        );
        assert_eq!(
            radix_order(200, |i| Some(i * 37 % 50), (Radix::Signed, true, false), None),
            [
                27, 77, 127, 177, 4, 54, 104, 154, 31, 131, 81, 181, 8, 58, 158, 108, 35, 85, 135,
                185, 62, 162, 12, 112, 39, 139, 89, 189, 16, 66, 166, 116, 43, 93, 143, 193, 20,
                70, 120, 170, 47, 147, 97, 197, 24, 74, 174, 124, 1, 51, 151, 101, 78, 178, 28,
                128, 5, 55, 155, 105, 32, 82, 182, 132, 9, 59, 159, 109, 86, 36, 136, 186, 13, 63,
                163, 113, 40, 90, 190, 140, 17, 67, 167, 117, 94, 194, 44, 144, 21, 71, 171, 121,
                98, 148, 48, 198, 25, 75, 175, 125, 2, 52, 102, 152, 29, 79, 179, 129, 6, 106, 156,
                56, 33, 83, 183, 133, 10, 110, 160, 60, 37, 87, 187, 137, 14, 114, 64, 164, 41, 91,
                191, 141, 18, 68, 118, 168, 45, 95, 195, 145, 22, 122, 172, 72, 49, 99, 199, 149,
                26, 126, 76, 176, 3, 53, 103, 153, 30, 130, 180, 80, 7, 107, 57, 157, 34, 84, 184,
                134, 11, 61, 111, 161, 38, 138, 188, 88, 15, 115, 65, 165, 42, 92, 142, 192, 19,
                69, 119, 169, 46, 146, 196, 96, 23, 123, 73, 173, 0, 50, 150, 100
            ]
        );
        let null = |i: i64| Some(i % 5).filter(|v| *v != 0);
        assert_eq!(
            radix_order(60, null, (Radix::Int32, false, true), None),
            [
                0, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55, 1, 16, 21, 31, 46, 56, 11, 6, 26, 41,
                51, 36, 17, 2, 22, 27, 7, 32, 42, 47, 12, 57, 37, 52, 13, 18, 23, 28, 33, 8, 43, 3,
                58, 48, 38, 53, 14, 4, 19, 29, 34, 44, 54, 59, 49, 9, 39, 24
            ]
        );
        assert_eq!(
            radix_order(60, null, int4, None),
            [
                1, 6, 11, 21, 26, 41, 56, 31, 46, 36, 16, 51, 2, 7, 12, 22, 27, 37, 42, 57, 47, 52,
                32, 17, 3, 8, 13, 23, 28, 38, 43, 58, 18, 33, 48, 53, 4, 9, 14, 19, 24, 29, 39, 44,
                59, 54, 34, 49, 0, 40, 50, 15, 5, 35, 20, 55, 45, 30, 25, 10
            ]
        );
        let wide = |i: i64| Some((i * 7919 % 1000) / 3 * 100_000 - 2_000_000);
        assert_eq!(
            radix_order(300, wide, (Radix::Signed, false, false), None),
            [
                0, 37, 74, 111, 148, 185, 222, 259, 296, 12, 49, 86, 123, 160, 197, 234, 271, 24,
                61, 98, 135, 172, 209, 246, 283, 36, 73, 110, 147, 184, 221, 258, 295, 11, 48, 85,
                122, 159, 196, 233, 270, 23, 60, 97, 134, 171, 208, 245, 282, 35, 72, 109, 146,
                183, 220, 257, 294, 10, 47, 84, 121, 158, 195, 232, 269, 22, 59, 96, 133, 170, 207,
                244, 281, 34, 71, 108, 145, 182, 219, 256, 293, 9, 46, 83, 120, 157, 194, 231, 268,
                21, 58, 95, 132, 169, 206, 243, 280, 33, 70, 107, 144, 181, 218, 255, 292, 8, 45,
                82, 119, 156, 193, 230, 267, 20, 57, 94, 131, 168, 205, 242, 279, 32, 69, 106, 143,
                180, 217, 254, 291, 7, 44, 81, 118, 155, 192, 229, 266, 19, 56, 93, 130, 167, 204,
                241, 278, 31, 68, 105, 142, 179, 216, 253, 290, 6, 43, 80, 117, 154, 191, 228, 265,
                18, 55, 92, 129, 166, 203, 240, 277, 30, 67, 104, 141, 178, 215, 252, 289, 5, 42,
                79, 116, 153, 190, 227, 264, 17, 54, 91, 128, 165, 202, 239, 276, 29, 66, 103, 140,
                177, 214, 251, 288, 4, 41, 78, 115, 152, 189, 226, 263, 16, 53, 90, 127, 164, 201,
                238, 275, 28, 65, 102, 139, 176, 213, 250, 287, 3, 40, 77, 114, 151, 188, 225, 262,
                299, 15, 52, 89, 126, 163, 200, 237, 274, 27, 64, 101, 138, 175, 212, 249, 286, 2,
                39, 76, 113, 150, 187, 224, 261, 298, 14, 51, 88, 125, 162, 199, 236, 273, 26, 63,
                100, 137, 174, 211, 248, 285, 1, 38, 75, 112, 149, 186, 223, 260, 297, 13, 50, 87,
                124, 161, 198, 235, 272, 25, 62, 99, 136, 173, 210, 247, 284
            ]
        );
        // Fewer than 40 items take `qsort`.
        assert_eq!(
            radix_order(35, |i| Some(i % 3), int4, None),
            [
                12, 33, 24, 15, 6, 21, 18, 3, 30, 9, 0, 27, 34, 1, 4, 7, 10, 13, 16, 19, 22, 25,
                28, 31, 14, 32, 26, 11, 5, 29, 8, 20, 17, 2, 23
            ]
        );
        // The nulls sort on the second key.
        assert_eq!(
            radix_order(60, null, int4, Some(|i| i % 3)),
            [
                26, 56, 11, 41, 1, 31, 46, 16, 51, 21, 6, 36, 2, 47, 32, 17, 22, 37, 52, 7, 12, 27,
                42, 57, 8, 38, 53, 23, 43, 13, 28, 58, 18, 33, 48, 3, 44, 29, 14, 59, 4, 19, 34,
                49, 9, 54, 39, 24, 5, 35, 50, 20, 10, 40, 55, 25, 45, 30, 15, 0
            ]
        );
        assert_eq!(
            radix_order(100, |i| Some(i % 7), int4, Some(|i| i % 3)),
            [
                35, 14, 98, 56, 77, 70, 91, 28, 49, 7, 63, 21, 42, 84, 0, 8, 50, 92, 29, 71, 43, 1,
                85, 22, 64, 15, 36, 57, 99, 78, 2, 23, 65, 86, 44, 79, 58, 16, 37, 51, 9, 30, 72,
                93, 38, 17, 80, 59, 94, 10, 52, 73, 31, 87, 3, 45, 66, 24, 53, 32, 95, 11, 74, 4,
                25, 67, 88, 46, 39, 18, 81, 60, 5, 26, 68, 89, 47, 82, 19, 61, 40, 12, 33, 54, 75,
                96, 41, 62, 20, 83, 55, 34, 13, 76, 97, 90, 27, 69, 48, 6
            ]
        );
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
