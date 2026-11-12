//! The filtered search of spec/13 section 13.4.4: a pre-filter scan when few rows pass the filter, the traversal of ACORN-1 between 1 and 50 percent, and a post-filter search with the iterative scan of pgvector 0.8 above. The thresholds are budgets until the benchmark sets them (question 54 of spec/24).

use crate::graph::{Filter, Graph, Visited};
use crate::metric::Metric;
use crate::rabitq::{Codes, EXACT_EPSILON, Quantizer, rank};
use crate::vectors::Vectors;
use crate::{Neighbor, exact};

/// The default of `rupg.vector_prefilter_rows`: a filter that passes fewer rows runs as a pre-filter scan.
pub const PREFILTER_ROWS: f64 = 50_000.0;

/// The thresholds of [`strategy`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    /// `rupg.vector_prefilter_rows`.
    pub prefilter_rows: f64,
    /// The least selectivity of a filtered traversal.
    pub traversal_low: f64,
    /// The most selectivity of a filtered traversal.
    pub traversal_high: f64,
}

impl Default for Thresholds {
    fn default() -> Thresholds {
        Thresholds { prefilter_rows: PREFILTER_ROWS, traversal_low: 0.01, traversal_high: 0.5 }
    }
}

/// The way of a search with a filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Evaluate the filter, then rank the passing rows by their codes and rerank, which is exact.
    PreFilter,
    /// Traverse the graph and expand through the nodes that fail the filter, as ACORN-1 does.
    Traversal,
    /// Search with no filter and test the filter after, with the iterative scan, until enough rows pass.
    PostFilter,
}

/// The strategy for a filter of the selectivity `selectivity` over `rows` rows. A filter that passes fewer than `prefilter_rows` rows, or less than `traversal_low` of them, runs as a pre-filter scan, since the traversal loses recall when few nodes pass.
#[must_use]
pub fn strategy(rows: f64, selectivity: f64, thresholds: &Thresholds) -> Strategy {
    if rows * selectivity < thresholds.prefilter_rows || selectivity < thresholds.traversal_low {
        Strategy::PreFilter
    } else if selectivity <= thresholds.traversal_high {
        Strategy::Traversal
    } else {
        Strategy::PostFilter
    }
}

/// The `k` nearest nodes of `passing` to `query`, by the codes `codes` and a rerank with the bounds of [`EXACT_EPSILON`] when the index has them and by an exact scan otherwise.
#[must_use]
pub fn pre_filter(
    vectors: &Vectors,
    metric: Metric,
    query: &[f32],
    passing: impl IntoIterator<Item = u32>,
    k: usize,
    codes: Option<(&Quantizer, &Codes)>,
) -> Vec<Neighbor> {
    match codes {
        Some((quantizer, codes)) => {
            rank(quantizer, codes, vectors, query, passing, k, EXACT_EPSILON).0
        }
        None => exact(vectors, metric, query, passing, k),
    }
}

/// The `k` nearest nodes to `query` that pass `filter`, by searches of the graph with no filter whose beam starts at `beam` and doubles, as the iterative scan of `hnsw` reads more of the graph, until `k` nodes pass, the beam reaches `max_scan` (`hnsw.max_scan_tuples`), or the search reaches each node.
#[must_use]
pub fn post_filter(
    graph: &Graph,
    vectors: &Vectors,
    query: &[f32],
    k: usize,
    beam: usize,
    max_scan: usize,
    filter: Filter<'_>,
) -> Vec<Neighbor> {
    let mut visited = Visited::default();
    let mut beam = beam.max(k).max(1);
    loop {
        let found = graph.search(vectors, query, beam, &mut visited);
        let exhausted = found.len() < beam;
        let mut passing: Vec<Neighbor> = found.into_iter().filter(|n| filter(n.node)).collect();
        if passing.len() >= k || exhausted || beam >= max_scan {
            passing.truncate(k);
            return passing;
        }
        beam = (beam * 2).min(max_scan.max(beam + 1));
    }
}
