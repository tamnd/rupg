//! The vector index of document 13, which `hnsw` and `ivfflat` map to (spec/13 section 13.4).
//!
//! The crate holds the methods of the index over vectors of `f32` in memory, with no storage of its own:
//!
//! - [`graph`]: the single-layer proximity graph of the Vamana method of DiskANN, which `USING hnsw` maps to, with the deleted marks and the consolidation of FreshDiskANN, and the filtered traversal of ACORN-1.
//! - [`ivf`]: the partitioned layout of `USING ivfflat`, whose lists come from the k-means of [`kmeans`].
//! - [`rabitq`]: the RaBitQ code of each vector, one bit for each dimension, with a distance estimate that has an error bound, and the rerank with the full vectors.
//! - [`index`]: the index over the versions of the rows of a table, which the scans of the sessions share, with the layout at the first search and the removal of the versions that `VACUUM` removes.
//! - [`filter`]: the choice between a pre-filter scan, a filtered traversal and a post-filter search from the selectivity of the filter (spec/13 section 13.4.4).
//!
//! This crate first ships in milestone M6. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![forbid(unsafe_code)]

pub mod filter;
pub mod graph;
pub mod index;
pub mod ivf;
pub mod kmeans;
pub mod metric;
pub mod rabitq;
mod rng;
pub mod vectors;

use std::cmp::Ordering;

pub use metric::Metric;
pub use vectors::Vectors;

/// A node of an index with its distance from the query of a search. The nodes are the numbers that [`Vectors::push`] gives.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Neighbor {
    /// The distance from the query, in the units of the [`Metric`] of the index.
    pub distance: f32,
    /// The node.
    pub node: u32,
}

impl Neighbor {
    /// The order of two neighbors: the nearer first, and the lower node first for the same distance, so that each search gives the same order.
    #[must_use]
    pub fn order(&self, other: &Neighbor) -> Ordering {
        self.distance.total_cmp(&other.distance).then(self.node.cmp(&other.node))
    }
}

/// A [`Neighbor`] in a heap, ordered by [`Neighbor::order`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Near(pub(crate) Neighbor);

impl Eq for Near {}

impl PartialOrd for Near {
    fn partial_cmp(&self, other: &Near) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Near {
    fn cmp(&self, other: &Near) -> Ordering {
        self.0.order(&other.0)
    }
}

/// The `k` nearest nodes of `candidates` to `query` by an exact scan, nearest first.
pub fn exact(
    vectors: &Vectors,
    metric: Metric,
    query: &[f32],
    candidates: impl IntoIterator<Item = u32>,
    k: usize,
) -> Vec<Neighbor> {
    let mut heap = std::collections::BinaryHeap::new();
    for node in candidates {
        let near = Near(Neighbor { distance: metric.distance(query, vectors.get(node)), node });
        if heap.len() < k {
            heap.push(near);
        } else if heap.peek().is_some_and(|worst| near < *worst) {
            heap.pop();
            heap.push(near);
        }
    }
    let mut out: Vec<Neighbor> = heap.into_iter().map(|n| n.0).collect();
    out.sort_by(Neighbor::order);
    out
}

/// The share of the nodes of `want` that `got` has, as recall at the length of `want`. An empty `want` has a recall of 1.
#[must_use]
pub fn recall(got: &[Neighbor], want: &[Neighbor]) -> f64 {
    if want.is_empty() {
        return 1.0;
    }
    let found = want.iter().filter(|w| got.iter().any(|g| g.node == w.node)).count();
    found as f64 / want.len() as f64
}

#[cfg(test)]
mod tests;
