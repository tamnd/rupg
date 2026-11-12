//! The k-means of the lists of the partitioned layout, as `ivfflatbuild` clusters the rows: a sample of at most `max(10000, 50 * lists)` vectors, the seeds of k-means++, and the iterations of Lloyd until no vector changes its center. For the inner product and the cosine the vectors and the centers lie on the unit sphere, as pgvector normalizes them with the norm function of the operator class.

use crate::metric::{Metric, dot, l2_squared, normalize};
use crate::rng::Rng;
use crate::vectors::Vectors;

/// The most iterations of Lloyd, as `ivfflat` has.
pub const MAX_ITERATIONS: usize = 500;

/// The size of the sample for `lists` lists: `max(10000, 50 * lists)`, as `ivfflatbuild` samples.
#[must_use]
pub fn sample_size(lists: usize) -> usize {
    10_000.max(lists.saturating_mul(50))
}

/// The distance by which k-means puts a vector in its list: the inner product on the sphere, and the Euclidean distance otherwise. Each gives the nearest center by the distance of the metric for a center on the sphere.
fn assign_distance(spherical: bool, a: &[f32], b: &[f32]) -> f32 {
    if spherical { -dot(a, b) } else { l2_squared(a, b) }
}

/// The centers of `k` lists of the vectors `vectors` by `metric`, from the seed `seed`, as `k * dim` values. With no more vectors than lists, each vector is the center of a list and the other centers are random, as `QuickCenters` makes them.
///
/// # Panics
///
/// When `k` is 0.
#[must_use]
pub fn centers(vectors: &Vectors, k: usize, metric: Metric, seed: u64) -> Vec<f32> {
    assert!(k > 0, "k-means with no lists");
    let dim = vectors.dim();
    let spherical = metric.spherical();
    let mut rng = Rng::new(seed);
    let n = vectors.len();
    // The sample: the first `size` nodes of a random order.
    let size = n.min(sample_size(k));
    let mut order = rng.permutation(n);
    order.truncate(size);
    order.sort_unstable();
    let mut points: Vec<f32> = Vec::with_capacity(size * dim);
    for &node in &order {
        points.extend_from_slice(vectors.get(node));
        if spherical {
            let at = points.len() - dim;
            normalize(&mut points[at..]);
        }
    }
    let point = |i: usize| &points[i * dim..(i + 1) * dim];
    if size <= k {
        let mut out = points.clone();
        while out.len() < k * dim {
            let at = out.len();
            out.extend((0..dim).map(|_| rng.unit() as f32));
            if spherical {
                normalize(&mut out[at..]);
            }
        }
        return out;
    }
    // The seeds of k-means++: each next center is a sample with a chance that grows with the square of its distance from the nearest center so far.
    let mut out: Vec<f32> = Vec::with_capacity(k * dim);
    out.extend_from_slice(point(rng.below(size)));
    let mut nearest: Vec<f64> =
        (0..size).map(|i| f64::from(l2_squared(point(i), &out[..dim]))).collect();
    while out.len() < k * dim {
        let total: f64 = nearest.iter().sum();
        let pick = if total > 0.0 {
            let mut target = rng.unit() * total;
            let mut pick = size - 1;
            for (i, &d) in nearest.iter().enumerate() {
                if target < d {
                    pick = i;
                    break;
                }
                target -= d;
            }
            pick
        } else {
            rng.below(size)
        };
        let at = out.len();
        out.extend_from_slice(point(pick));
        for (i, d) in nearest.iter_mut().enumerate() {
            *d = d.min(f64::from(l2_squared(point(i), &out[at..at + dim])));
        }
    }
    // The iterations of Lloyd.
    let mut assigned = vec![usize::MAX; size];
    let mut sums = vec![0f64; k * dim];
    let mut counts = vec![0usize; k];
    for _ in 0..MAX_ITERATIONS {
        let mut changed = false;
        for (i, slot) in assigned.iter_mut().enumerate() {
            let best = nearest_center(&out, dim, point(i), spherical);
            if *slot != best {
                *slot = best;
                changed = true;
            }
        }
        if !changed {
            break;
        }
        sums.fill(0.0);
        counts.fill(0);
        for (i, &c) in assigned.iter().enumerate() {
            counts[c] += 1;
            for (s, &x) in sums[c * dim..(c + 1) * dim].iter_mut().zip(point(i)) {
                *s += f64::from(x);
            }
        }
        for c in 0..k {
            let center = &mut out[c * dim..(c + 1) * dim];
            if counts[c] == 0 {
                // An empty list takes a random sample as its center.
                center.copy_from_slice(point(rng.below(size)));
                continue;
            }
            for (x, &s) in center.iter_mut().zip(&sums[c * dim..(c + 1) * dim]) {
                *x = (s / counts[c] as f64) as f32;
            }
            if spherical {
                normalize(center);
            }
        }
    }
    out
}

/// The list of `centers`, each of `dim` values, whose center is nearest to `v` for k-means.
pub(crate) fn nearest_center(centers: &[f32], dim: usize, v: &[f32], spherical: bool) -> usize {
    let mut best = (f32::INFINITY, 0);
    for (c, center) in centers.chunks_exact(dim).enumerate() {
        let d = assign_distance(spherical, v, center);
        if d < best.0 {
            best = (d, c);
        }
    }
    best.1
}
