//! The partitioned layout of `USING ivfflat` (spec/13 section 13.4.2): the vectors in lists by the nearest center of k-means, and a search of the nearest `probes` lists. An insert goes to the list of its nearest center, and the centers stay as the build made them, as in pgvector.

use crate::kmeans;
use crate::metric::Metric;
use crate::vectors::Vectors;
use crate::{Neighbor, exact};

/// The lists of a partitioned index.
#[derive(Clone, Debug)]
pub struct Ivf {
    metric: Metric,
    dim: usize,
    /// The center of each list, `dim` values each.
    centers: Vec<f32>,
    /// The nodes of each list.
    lists: Vec<Vec<u32>>,
}

impl Ivf {
    /// The index of `lists` lists over the vectors `vectors` by `metric`, from the seed `seed` of k-means.
    ///
    /// # Panics
    ///
    /// When `lists` is 0.
    #[must_use]
    pub fn build(vectors: &Vectors, lists: usize, metric: Metric, seed: u64) -> Ivf {
        let centers = kmeans::centers(vectors, lists, metric, seed);
        let mut ivf = Ivf { metric, dim: vectors.dim(), centers, lists: vec![Vec::new(); lists] };
        for node in vectors.nodes() {
            ivf.insert(vectors, node);
        }
        ivf
    }

    /// Adds the node `node` of `vectors` to the list of its nearest center.
    pub fn insert(&mut self, vectors: &Vectors, node: u32) {
        let list = self.nearest_lists(vectors.get(node), 1)[0];
        self.lists[list].push(node);
    }

    /// Takes the node `node` out of its list, as `VACUUM` does with a dead row.
    pub fn remove(&mut self, node: u32) {
        for list in &mut self.lists {
            list.retain(|&n| n != node);
        }
    }

    /// Keeps the nodes for which `keep` gives true, as `VACUUM` takes out the dead rows.
    pub fn retain(&mut self, keep: impl Fn(u32) -> bool) {
        for list in &mut self.lists {
            list.retain(|&n| keep(n));
        }
    }

    /// The number of lists.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lists.len()
    }

    /// True when the index has no lists.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lists.is_empty()
    }

    /// The nodes of the list `list`.
    #[must_use]
    pub fn list(&self, list: usize) -> &[u32] {
        &self.lists[list]
    }

    /// The center of the list `list`.
    #[must_use]
    pub fn center(&self, list: usize) -> &[f32] {
        &self.centers[list * self.dim..(list + 1) * self.dim]
    }

    /// The `probes` lists whose centers are nearest to `query` by the metric of the index, nearest first, as `ivfflatgettuple` picks the lists.
    #[must_use]
    pub fn nearest_lists(&self, query: &[f32], probes: usize) -> Vec<usize> {
        let mut by: Vec<(f32, usize)> = self
            .centers
            .chunks_exact(self.dim)
            .enumerate()
            .map(|(c, center)| (self.metric.distance(query, center), c))
            .collect();
        by.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        by.truncate(probes.max(1));
        by.into_iter().map(|(_, c)| c).collect()
    }

    /// The nodes of the `probes` lists nearest to `query`, which a scan of the index reads.
    #[must_use]
    pub fn candidates(&self, query: &[f32], probes: usize) -> Vec<u32> {
        self.nearest_lists(query, probes)
            .into_iter()
            .flat_map(|l| self.lists[l].iter().copied())
            .collect()
    }

    /// The `k` nearest nodes to `query` in the `probes` nearest lists, nearest first.
    #[must_use]
    pub fn search(
        &self,
        vectors: &Vectors,
        query: &[f32],
        probes: usize,
        k: usize,
    ) -> Vec<Neighbor> {
        exact(vectors, self.metric, query, self.candidates(query, probes), k)
    }
}
