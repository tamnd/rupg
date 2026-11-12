//! The distances of the operator classes of pgvector over vectors of `f32`. Each distance orders the nodes as the distance operator of its operator class does: `<->` by [`Metric::L2`], `<#>` by [`Metric::InnerProduct`], `<=>` by [`Metric::Cosine`] and `<+>` by [`Metric::L1`].

/// The distance of an index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Metric {
    /// The square of the Euclidean distance, which orders as `<->` does.
    L2,
    /// The negative inner product of `<#>`.
    InnerProduct,
    /// The cosine distance of `<=>`. A vector with a norm of zero has a distance of 1.
    Cosine,
    /// The taxicab distance of `<+>`.
    L1,
}

impl Metric {
    /// The distance between `a` and `b`, which have the same length.
    #[must_use]
    pub fn distance(self, a: &[f32], b: &[f32]) -> f32 {
        match self {
            Metric::L2 => l2_squared(a, b),
            Metric::InnerProduct => -dot(a, b),
            Metric::Cosine => {
                let norms = (dot(a, a) * dot(b, b)).sqrt();
                if norms == 0.0 { 1.0 } else { 1.0 - dot(a, b) / norms }
            }
            Metric::L1 => a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum(),
        }
    }

    /// True when the distance is never negative and the robust prune of [`crate::graph`] can scale it by its `alpha`.
    #[must_use]
    pub fn scalable(self) -> bool {
        !matches!(self, Metric::InnerProduct)
    }

    /// True when k-means clusters the vectors on the unit sphere, as `ivfflat` does for an operator class with a norm function: the inner product and the cosine.
    #[must_use]
    pub fn spherical(self) -> bool {
        matches!(self, Metric::InnerProduct | Metric::Cosine)
    }
}

/// The square of the Euclidean distance between `a` and `b`.
#[must_use]
pub fn l2_squared(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// The inner product of `a` and `b`.
#[must_use]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// `v` scaled to a norm of 1, or `v` itself when its norm is zero.
pub fn normalize(v: &mut [f32]) {
    let norm = dot(v, v).sqrt();
    if norm > 0.0 {
        for x in v {
            *x /= norm;
        }
    }
}
