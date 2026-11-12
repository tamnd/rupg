//! The RaBitQ code of spec/13 section 13.4.3 (Gao and Long, SIGMOD 2024): one bit for each dimension, the sign of each value of the vector after a random rotation about a center, with a distance estimate that has a proven error bound. A search ranks the candidates by the lower bound of the estimate and reranks with the full vector each candidate whose bound reaches below the current k-th distance, and drops the others with no read of the full vector. A code of 1536 dimensions is 192 bytes.
//!
//! The rotation is orthogonal and costs `O(D log D)`: rounds of random signs and the normalized Walsh-Hadamard transform over the first and the last `2^⌊log2 D⌋` values, which overlap and so cover each value. The cosine encodes the vectors scaled to a norm of 1, and the inner product comes from the Euclidean estimate and the norms. The taxicab distance has no estimate.

use std::collections::BinaryHeap;

use crate::metric::{Metric, dot, normalize};
use crate::rng::Rng;
use crate::vectors::Vectors;
use crate::{Near, Neighbor};

/// `ε0` of the error bound of [`Estimate::error`], for which the bound of a distance holds with a probability near 0.97.
pub const EPSILON0: f32 = 1.9;

/// The `ε0` of a rank that must be exact, as the pre-filter scan is: with it the bound fails for about one distance in a million, and a rank reads more full vectors.
pub const EXACT_EPSILON: f32 = 5.0;

/// The rounds of the rotation. Six rounds give bounds that hold nearly as often as those of a uniform random rotation.
const ROUNDS: usize = 6;

/// The rotation and the center of a set of codes.
#[derive(Clone, Debug)]
pub struct Quantizer {
    metric: Metric,
    dim: usize,
    /// The size of the blocks of the transform: the largest power of two no larger than `dim`.
    block: usize,
    center: Vec<f32>,
    /// The signs of each round, `dim` for each.
    signs: Vec<f32>,
}

/// The codes of the vectors of an index, by node.
#[derive(Clone, Debug, Default)]
pub struct Codes {
    /// The words of 64 bits of each code.
    words: usize,
    bits: Vec<u64>,
    /// `‖o - c‖` of each vector.
    norms: Vec<f32>,
    /// `⟨x̄, x⟩` of each vector: the inner product of the code with the rotated unit vector.
    dots: Vec<f32>,
    /// `‖o‖²` of each vector, for the inner product.
    squares: Vec<f32>,
}

impl Codes {
    /// The number of codes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.norms.len()
    }

    /// True when there are no codes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.norms.is_empty()
    }
}

/// A query, rotated for the estimates.
#[derive(Clone, Debug)]
pub struct Query {
    /// The rotated unit vector of `q - c`.
    rotated: Vec<f32>,
    /// The sum of [`Query::rotated`].
    sum: f32,
    /// `‖q - c‖`.
    norm: f32,
    /// `‖q‖²`.
    square: f32,
}

/// An estimate of a distance, in the units of the metric.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Estimate {
    /// The estimate.
    pub distance: f32,
    /// The bound of the error: the distance is within `error` of the estimate with a high probability.
    pub error: f32,
}

/// The normalized Walsh-Hadamard transform of `x`, whose length is a power of two.
fn hadamard(x: &mut [f32]) {
    let n = x.len();
    let mut h = 1;
    while h < n {
        for i in (0..n).step_by(2 * h) {
            for j in i..i + h {
                let (a, b) = (x[j], x[j + h]);
                x[j] = a + b;
                x[j + h] = a - b;
            }
        }
        h *= 2;
    }
    let scale = 1.0 / (n as f32).sqrt();
    for v in x {
        *v *= scale;
    }
}

impl Quantizer {
    /// The quantizer of vectors of `center.len()` dimensions about `center`, with the rotation of the seed `seed`, or `None` for [`Metric::L1`] or no dimensions.
    #[must_use]
    pub fn new(metric: Metric, center: Vec<f32>, seed: u64) -> Option<Quantizer> {
        let dim = center.len();
        if metric == Metric::L1 || dim == 0 {
            return None;
        }
        let block = 1usize << (usize::BITS - 1 - dim.leading_zeros());
        let mut rng = Rng::new(seed);
        let signs =
            (0..ROUNDS * dim).map(|_| if rng.next_u64() & 1 == 0 { 1.0 } else { -1.0 }).collect();
        Some(Quantizer { metric, dim, block, center, signs })
    }

    /// The quantizer of `vectors` about their mean, or `None` as for [`Quantizer::new`].
    #[must_use]
    pub fn fit(vectors: &Vectors, metric: Metric, seed: u64) -> Option<Quantizer> {
        let dim = vectors.dim();
        let mut mean = vec![0f64; dim];
        let mut v = vec![0f32; dim];
        for node in vectors.nodes() {
            v.copy_from_slice(vectors.get(node));
            if metric == Metric::Cosine {
                normalize(&mut v);
            }
            for (m, &x) in mean.iter_mut().zip(&v) {
                *m += f64::from(x);
            }
        }
        let count = vectors.len().max(1) as f64;
        Quantizer::new(metric, mean.into_iter().map(|m| (m / count) as f32).collect(), seed)
    }

    /// The number of bytes of a code.
    #[must_use]
    pub fn code_bytes(&self) -> usize {
        self.dim.div_ceil(8)
    }

    /// The codes of `vectors`.
    #[must_use]
    pub fn encode_all(&self, vectors: &Vectors) -> Codes {
        let mut codes = Codes::default();
        for node in vectors.nodes() {
            self.encode(vectors.get(node), &mut codes);
        }
        codes
    }

    /// The rotation of `v - c`, for `v` scaled to a norm of 1 for the cosine, as a unit vector, with `‖v - c‖` and `‖v‖²`.
    fn rotate(&self, v: &[f32]) -> (Vec<f32>, f32, f32) {
        let mut x = v.to_vec();
        if self.metric == Metric::Cosine {
            normalize(&mut x);
        }
        let square = dot(&x, &x);
        for (a, &c) in x.iter_mut().zip(&self.center) {
            *a -= c;
        }
        for round in 0..ROUNDS {
            for (a, &s) in x.iter_mut().zip(&self.signs[round * self.dim..(round + 1) * self.dim]) {
                *a *= s;
            }
            if round % 2 == 0 {
                hadamard(&mut x[..self.block]);
            } else {
                hadamard(&mut x[self.dim - self.block..]);
            }
        }
        let norm = dot(&x, &x).sqrt();
        if norm > 0.0 {
            for a in &mut x {
                *a /= norm;
            }
        }
        (x, norm, square)
    }

    /// Adds the code of `v` to `codes`.
    pub fn encode(&self, v: &[f32], codes: &mut Codes) {
        let (x, norm, square) = self.rotate(v);
        codes.words = self.dim.div_ceil(64);
        let at = codes.bits.len();
        codes.bits.resize(at + codes.words, 0);
        let mut sum = 0f32;
        for (i, &value) in x.iter().enumerate() {
            if value > 0.0 {
                codes.bits[at + i / 64] |= 1 << (i % 64);
            }
            sum += value.abs();
        }
        codes.norms.push(norm);
        codes.dots.push(sum / (self.dim as f32).sqrt());
        codes.squares.push(square);
    }

    /// The query `q`, rotated for [`Quantizer::estimate`].
    #[must_use]
    pub fn query(&self, q: &[f32]) -> Query {
        let (rotated, norm, square) = self.rotate(q);
        let sum = rotated.iter().sum();
        Query { rotated, sum, norm, square }
    }

    /// The estimate of the distance between `query` and the vector of the code `node`, with its error bound.
    #[must_use]
    pub fn estimate(&self, codes: &Codes, node: u32, query: &Query) -> Estimate {
        let n = node as usize;
        let bits = &codes.bits[n * codes.words..(n + 1) * codes.words];
        let mut positive = 0f32;
        for (w, &word) in bits.iter().enumerate() {
            let mut word = word;
            while word != 0 {
                let i = w * 64 + word.trailing_zeros() as usize;
                positive += query.rotated[i];
                word &= word - 1;
            }
        }
        let root = (self.dim as f32).sqrt();
        let code_dot = codes.dots[n];
        // `⟨x̄, q'⟩`, with `x̄` of `±1/√D`, over `⟨x̄, x⟩` estimates the inner product of the unit vectors of `o - c` and `q - c`.
        let (inner, bound) = if code_dot > 0.0 {
            let estimate = ((2.0 * positive - query.sum) / root / code_dot).clamp(-1.0, 1.0);
            let spread = ((1.0 - code_dot * code_dot).max(0.0) / (code_dot * code_dot)).sqrt();
            (estimate, (spread * EPSILON0 / (self.dim as f32 - 1.0).max(1.0).sqrt()).min(2.0))
        } else {
            (0.0, 2.0)
        };
        let scale = 2.0 * codes.norms[n] * query.norm;
        let l2 =
            (codes.norms[n] * codes.norms[n] + query.norm * query.norm - scale * inner).max(0.0);
        let error = scale * bound;
        match self.metric {
            Metric::L2 | Metric::L1 => Estimate { distance: l2, error },
            Metric::Cosine => Estimate { distance: l2 / 2.0, error: error / 2.0 },
            Metric::InnerProduct => Estimate {
                distance: (l2 - codes.squares[n] - query.square) / 2.0,
                error: error / 2.0,
            },
        }
    }
}

/// The `k` nearest nodes of `candidates` to `query`, by the lower bounds of their estimates with the `ε0` of `epsilon` and a rerank with the full vectors of `vectors` until the next lower bound is above the k-th distance, nearest first, with the number of candidates that the rerank read.
#[must_use]
pub fn rank(
    quantizer: &Quantizer,
    codes: &Codes,
    vectors: &Vectors,
    query: &[f32],
    candidates: impl IntoIterator<Item = u32>,
    k: usize,
    epsilon: f32,
) -> (Vec<Neighbor>, usize) {
    if k == 0 {
        return (Vec::new(), 0);
    }
    let q = quantizer.query(query);
    let mut bounds: Vec<(f32, u32)> = candidates
        .into_iter()
        .map(|n| {
            let e = quantizer.estimate(codes, n, &q);
            (e.distance - e.error * (epsilon / EPSILON0), n)
        })
        .collect();
    bounds.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut best: BinaryHeap<Near> = BinaryHeap::new();
    let mut read = 0;
    for (lower, node) in bounds {
        if best.len() >= k && best.peek().is_some_and(|w| lower > w.0.distance) {
            break;
        }
        read += 1;
        best.push(Near(Neighbor {
            distance: quantizer.metric.distance(query, vectors.get(node)),
            node,
        }));
        if best.len() > k {
            best.pop();
        }
    }
    let mut out: Vec<Neighbor> = best.into_iter().map(|n| n.0).collect();
    out.sort_by(Neighbor::order);
    (out, read)
}
