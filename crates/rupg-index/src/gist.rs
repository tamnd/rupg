//! The parts of a `gist` scan whose results differ from a test of each row with the operator: the order of the leaf keys that the sorted build of `point_ops` gives, the order in which the queue of an ordered scan gives rows with equal distances, and the fuzzy bounding box test that `gist_point_consistent` makes before it tests a point in a polygon or a circle.

use std::cmp::Ordering;

/// `EPSILON` of `geo_decls.h`.
pub const EPSILON: f64 = 1.0e-6;

/// `ieee_float32_to_uint32`: a number whose order as an unsigned integer is the order of the `float4`, with NaN above all others.
fn float32_key(f: f32) -> u32 {
    if f.is_nan() {
        return u32::MAX;
    }
    let bits = f.to_bits();
    if bits & 0x8000_0000 != 0 { bits ^ u32::MAX } else { bits | 0x8000_0000 }
}

/// `part_bits32_by2`: the bits of `x` at the even places of the result.
fn spread(x: u32) -> u64 {
    let mut n = u64::from(x);
    n = (n | (n << 16)) & 0x0000_FFFF_0000_FFFF;
    n = (n | (n << 8)) & 0x00FF_00FF_00FF_00FF;
    n = (n | (n << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
    n = (n | (n << 2)) & 0x3333_3333_3333_3333;
    (n | (n << 1)) & 0x5555_5555_5555_5555
}

/// `point_zorder_internal`: the Z-order of a point, from its coordinates as `float4`, with the bits of `y` above the bits of `x` at each place. `gist_bbox_zorder_cmp` orders the leaf keys of the sorted build of `point_ops` by it, and the build sorts equal keys by the heap TID.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn point_zorder(x: f64, y: f64) -> u64 {
    spread(float32_key(x as f32)) | (spread(float32_key(y as f32)) << 1)
}

/// `box_ov` with `FPle`: true when the boxes `(low x, low y, high x, high y)` overlap within [`EPSILON`]. `gist_poly_consistent` and `gist_circle_consistent` test the bounding box of the query with it, also on a leaf.
#[must_use]
pub fn box_overlap(a: [f64; 4], b: [f64; 4]) -> bool {
    let le = |x: f64, y: f64| x - y <= EPSILON;
    le(a[0], b[2]) && le(b[0], a[2]) && le(a[1], b[3]) && le(b[1], a[3])
}

/// `float8_lt`: NaN is above all numbers.
fn float8_lt(a: f64, b: f64) -> bool {
    !a.is_nan() && (b.is_nan() || a < b)
}

/// The bounding box `(low x, low y, high x, high y)` of the query of a condition `point <@ polygon` or `point <@ circle`, which `gist_poly_consistent` or `gist_circle_consistent` tests with [`box_overlap`]: `make_bound_box` of the points of the binary form of a `polygon`, the number of points and then the points, or the center plus and minus the radius of the binary form of a `circle`. `None` for bytes that are not such a form.
#[must_use]
pub fn query_box(polygon: bool, bytes: &[u8]) -> Option<[f64; 4]> {
    let float = |chunk: &[u8; 8]| f64::from_be_bytes(*chunk);
    if !polygon {
        let (chunks, rest) = bytes.as_chunks::<8>();
        let [x, y, r] = chunks else { return None };
        let (x, y, r) = (float(x), float(y), float(r));
        return rest.is_empty().then_some([x - r, y - r, x + r, y + r]);
    }
    let (npts, rest) = bytes.split_first_chunk::<4>()?;
    let (chunks, tail) = rest.as_chunks::<8>();
    let (pairs, odd) = chunks.as_chunks::<2>();
    let npts = usize::try_from(i32::from_be_bytes(*npts)).ok()?;
    if npts != pairs.len() || npts == 0 || !tail.is_empty() || !odd.is_empty() {
        return None;
    }
    let (x0, y0) = (float(&pairs[0][0]), float(&pairs[0][1]));
    let mut b = [x0, y0, x0, y0];
    for [x, y] in &pairs[1..] {
        let (x, y) = (float(x), float(y));
        if float8_lt(x, b[0]) {
            b[0] = x;
        }
        if float8_lt(b[2], x) {
            b[2] = x;
        }
        if float8_lt(y, b[1]) {
            b[1] = y;
        }
        if float8_lt(b[3], y) {
            b[3] = y;
        }
    }
    Some(b)
}

/// A node of the pairing heap of [`pairing_order`].
struct Node {
    first: Option<usize>,
    next: Option<usize>,
}

/// The pairing heap of `pairingheap.c` over the items of a list, by their places.
struct Heap<'a, T, F> {
    items: &'a [T],
    nodes: Vec<Node>,
    order: F,
}

impl<T, E, F: FnMut(&T, &T) -> Result<Ordering, E>> Heap<'_, T, F> {
    /// `merge`: the root of the heap of the two heaps. `b` goes under `a` unless `b` comes first, so the older root stays the root when the two are equal.
    fn merge(&mut self, a: Option<usize>, b: Option<usize>) -> Result<Option<usize>, E> {
        let (Some(mut a), Some(mut b)) = (a, b) else { return Ok(a.or(b)) };
        if (self.order)(&self.items[a], &self.items[b])? == Ordering::Greater {
            std::mem::swap(&mut a, &mut b);
        }
        self.nodes[b].next = self.nodes[a].first;
        self.nodes[a].first = Some(b);
        Ok(Some(a))
    }

    /// `merge_children`: the two passes over the children of a removed root, first in pairs from left to right, then the pairs in the order that the first pass made.
    fn merge_children(&mut self, children: Option<usize>) -> Result<Option<usize>, E> {
        let Some(first) = children else { return Ok(None) };
        if self.nodes[first].next.is_none() {
            return Ok(children);
        }
        let mut pairs: Option<usize> = None;
        let mut next = children;
        while let Some(curr) = next {
            let Some(second) = self.nodes[curr].next else {
                self.nodes[curr].next = pairs;
                pairs = Some(curr);
                break;
            };
            next = self.nodes[second].next;
            let merged = self.merge(Some(curr), Some(second))?.unwrap_or(curr);
            self.nodes[merged].next = pairs;
            pairs = Some(merged);
        }
        let mut root = pairs;
        let mut next = pairs.and_then(|p| self.nodes[p].next);
        while let Some(curr) = next {
            next = self.nodes[curr].next;
            root = self.merge(root, Some(curr))?;
        }
        Ok(root)
    }
}

/// The order in which the queue of an ordered `gist` scan gives the items of a leaf page that it took in the order of the list: `pairingheap_add` of each item, then `pairingheap_remove_first` until the queue is empty. `order` gives `Less` when its first item comes first, as `pairingheap_GISTSearchItem_cmp` with the sign reversed. Items that `order` finds equal do not keep the order of the list.
///
/// # Errors
///
/// The first error of `order`.
pub fn pairing_order<T, E>(
    items: Vec<T>,
    order: impl FnMut(&T, &T) -> Result<Ordering, E>,
) -> Result<Vec<T>, E> {
    let n = items.len();
    let mut heap = Heap {
        items: &items,
        nodes: (0..n).map(|_| Node { first: None, next: None }).collect(),
        order,
    };
    let mut root = None;
    for i in 0..n {
        root = heap.merge(root, Some(i))?;
        if let Some(r) = root {
            heap.nodes[r].next = None;
        }
    }
    let mut out = Vec::with_capacity(n);
    while let Some(r) = root {
        out.push(r);
        let children = heap.nodes[r].first.take();
        root = heap.merge_children(children)?;
        if let Some(r) = root {
            heap.nodes[r].next = None;
        }
    }
    let mut slots: Vec<Option<T>> = items.into_iter().map(Some).collect();
    Ok(out.into_iter().filter_map(|i| slots[i].take()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    type Point = Option<(f64, f64)>;

    /// The rows of `point_tbl` of the regression tests of PostgreSQL in the order of the heap, with the null row of `create_index.sql`.
    fn points() -> Vec<Point> {
        let inf = f64::INFINITY;
        vec![
            Some((0.0, 0.0)),
            Some((-10.0, 0.0)),
            Some((-3.0, 4.0)),
            Some((5.1, 34.5)),
            Some((-5.0, -12.0)),
            Some((1e-300, -1e-300)),
            Some((1e300, inf)),
            Some((inf, 1e300)),
            Some((f64::NAN, f64::NAN)),
            Some((10.0, 10.0)),
            None,
        ]
    }

    /// The distance of a point to `(0,1)`, as the order of an ordered scan compares it: null last, NaN above the numbers.
    fn by_distance(a: &(usize, Point), b: &(usize, Point)) -> Result<Ordering, ()> {
        let d = |p: &Point| p.map(|(x, y)| x.hypot(y - 1.0));
        Ok(match (d(&a.1), d(&b.1)) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(x), Some(y)) => x.total_cmp(&y),
        })
    }

    #[test]
    fn sorted_build_and_queue_give_the_order_of_postgres() {
        let mut leaf: Vec<(usize, Point)> = points().into_iter().enumerate().collect();
        leaf.sort_by_key(|(i, p)| (p.is_none(), p.map_or(0, |(x, y)| point_zorder(x, y)), *i));
        let order = pairing_order(leaf, by_distance).unwrap();
        let rows: Vec<usize> = order.into_iter().map(|(i, _)| i).collect();
        // `SELECT * FROM point_tbl ORDER BY f1 <-> '0,1'` of `create_index.out`.
        assert_eq!(rows, [5, 0, 2, 1, 9, 4, 3, 7, 6, 8, 10]);
    }

    #[test]
    fn equal_items_after_the_first_change_places() {
        let items: Vec<(usize, u8)> = (0..6).map(|i| (i, 0)).collect();
        let order = pairing_order(items, |a, b| Ok::<_, ()>(a.1.cmp(&b.1))).unwrap();
        assert_eq!(order[0].0, 0);
        let mut rows: Vec<usize> = order.iter().map(|(i, _)| *i).collect();
        rows.sort_unstable();
        assert_eq!(rows, [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn distinct_items_come_sorted() {
        let items: Vec<i32> = vec![5, 3, 9, 1, 7, 2, 8];
        let order = pairing_order(items, |a, b| Ok::<_, ()>(a.cmp(b))).unwrap();
        assert_eq!(order, [1, 2, 3, 5, 7, 8, 9]);
    }

    #[test]
    fn fuzzy_box_overlap() {
        let polygon = [0.0, 0.0, 100.0, 100.0];
        assert!(box_overlap([1e-300, -1e-300, 1e-300, -1e-300], polygon));
        assert!(!box_overlap([1e300, f64::INFINITY, 1e300, f64::INFINITY], polygon));
        assert!(!box_overlap([f64::NAN, f64::NAN, f64::NAN, f64::NAN], polygon));
        assert!(box_overlap([100.000_000_5, 50.0, 100.000_000_5, 50.0], polygon));
    }

    #[test]
    fn boxes_of_polygons_and_circles() {
        let mut polygon = 3_i32.to_be_bytes().to_vec();
        for v in [0.0_f64, 0.0, 100.0, 50.0, 20.0, -5.0] {
            polygon.extend(v.to_be_bytes());
        }
        assert_eq!(query_box(true, &polygon), Some([0.0, -5.0, 100.0, 50.0]));
        assert_eq!(query_box(true, &polygon[..20]), None);
        let circle: Vec<u8> = [50.0_f64, 50.0, 50.0].iter().flat_map(|v| v.to_be_bytes()).collect();
        assert_eq!(query_box(false, &circle), Some([0.0, 0.0, 100.0, 100.0]));
    }

    #[test]
    fn zorder_puts_negative_zero_first() {
        assert!(point_zorder(1e-300, -1e-300) < point_zorder(0.0, 0.0));
        assert_eq!(point_zorder(1e300, f64::INFINITY), point_zorder(f64::INFINITY, 1e300));
    }
}
