//! `pg_qsort` of `sort_template.h`. Code that must give equal items in the same order as PostgreSQL uses it in place of the sort of the standard library.

use std::cmp::Ordering;

/// `med3`: the index of the median of three items.
fn med3<T, E, F>(data: &[T], a: usize, b: usize, c: usize, cmp: &mut F) -> Result<usize, E>
where
    F: FnMut(&T, &T) -> Result<Ordering, E>,
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

/// `pg_qsort`: a quicksort with the median of three or of nine, a partition in three parts and an insertion sort for fewer than 7 items. It is not stable. The sort stops at the first error of `cmp`.
///
/// # Errors
///
/// The first error of `cmp`.
pub fn qsort<T, E, F>(data: &mut [T], cmp: &mut F) -> Result<(), E>
where
    F: FnMut(&T, &T) -> Result<Ordering, E>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The order of the keys only, so that a test can see which of two equal items comes first.
    fn by_key(a: &(i32, usize), b: &(i32, usize)) -> Result<Ordering, ()> {
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

    #[test]
    fn same_order_as_postgres() {
        // The order comes from the qsort of sort_template.h, compiled in C.
        let mut items: Vec<(i32, usize)> =
            (0..10).map(|i| (i % 2, usize::try_from(i).unwrap())).collect();
        qsort(&mut items, &mut by_key).unwrap();
        assert_eq!(items.iter().map(|i| i.1).collect::<Vec<_>>(), [2, 6, 4, 8, 0, 9, 1, 3, 5, 7]);
    }
}
