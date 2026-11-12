//! An index of [`graph`](crate::graph) or [`ivf`](crate::ivf) over the versions of the rows of a table, by the IDs of the versions, which the scans of the sessions share.
//!
//! A scan adds each version that the index does not know, with its vector or with none for a version that the index does not hold, such as one whose key is null. The layout comes at the first search and grows with each later one, and `VACUUM` takes out the versions that it removes, so that an ID that it gives again to a new version is new to the index.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasherDefault;

use crate::graph::{Graph, Params, Visited};
use crate::ivf::Ivf;
use crate::{Metric, Vectors, kmeans};

/// A hasher with no random state, so that each run is the same.
type Fixed = BuildHasherDefault<DefaultHasher>;

/// The layout of an [`Index`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The graph of `USING hnsw`, with its options `m` and `ef_construction`.
    Graph { m: usize, ef_construction: usize },
    /// The lists of `USING ivfflat`, with its option `lists`.
    Ivf { lists: usize },
}

/// The layout of the nodes that [`Index::lay`] placed.
#[derive(Clone, Debug)]
enum Layout {
    None,
    Graph(Graph),
    Ivf(Ivf),
}

/// The vectors of the versions of the rows of a table and their layout.
#[derive(Clone, Debug)]
pub struct Index {
    kind: Kind,
    metric: Metric,
    seed: u64,
    /// The vectors, with the dimensions of the first one, or `None` before the first.
    vectors: Option<Vectors>,
    /// The node of each version that the index holds.
    nodes: HashMap<u64, u32, Fixed>,
    /// The versions that the index knows and does not hold.
    skipped: HashSet<u64, Fixed>,
    /// True for each node whose version `VACUUM` took out.
    gone: Vec<bool>,
    /// The nodes that are gone.
    removed: usize,
    layout: Layout,
    /// The nodes before this one are in the layout.
    laid: u32,
    /// The nodes of the lists when k-means made their centers.
    built: usize,
    /// True after a vector whose dimensions differ from the first, so that the scan reads the rows as it does with no index.
    unusable: bool,
}

impl Index {
    /// An index of `kind` by `metric` with no versions, whose layout comes from the seed `seed`.
    #[must_use]
    pub fn new(kind: Kind, metric: Metric, seed: u64) -> Index {
        Index {
            kind,
            metric,
            seed,
            vectors: None,
            nodes: HashMap::default(),
            skipped: HashSet::default(),
            gone: Vec::new(),
            removed: 0,
            layout: Layout::None,
            laid: 0,
            built: 0,
            unusable: false,
        }
    }

    /// The layout of the index.
    #[must_use]
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The metric of the index.
    #[must_use]
    pub fn metric(&self) -> Metric {
        self.metric
    }

    /// True when the index has the version `id`, with a vector or with none.
    #[must_use]
    pub fn knows(&self, id: u64) -> bool {
        self.nodes.contains_key(&id) || self.skipped.contains(&id)
    }

    /// True when the index holds the version `id` with a vector.
    #[must_use]
    pub fn holds(&self, id: u64) -> bool {
        self.nodes.contains_key(&id)
    }

    /// The versions that the index holds with a vector.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True when the index holds no vector.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// True when a vector of other dimensions than the first came, so that the index gives no search.
    #[must_use]
    pub fn unusable(&self) -> bool {
        self.unusable
    }

    /// Adds the version `id` with its vector `vector`, or with none for a version that the index does not hold. A version that the index knows stays as it is.
    pub fn add(&mut self, id: u64, vector: Option<&[f32]>) {
        if self.knows(id) {
            return;
        }
        let Some(vector) = vector else {
            self.skipped.insert(id);
            return;
        };
        let vectors = self.vectors.get_or_insert_with(|| Vectors::new(vector.len()));
        if vectors.dim() != vector.len()
            || vector.is_empty()
            || u32::try_from(vectors.len()).is_err()
        {
            self.unusable = true;
            self.skipped.insert(id);
            return;
        }
        let node = vectors.push(id, vector);
        self.gone.push(false);
        self.nodes.insert(id, node);
    }

    /// Takes the versions `ids` out of the index, as `VACUUM` removes them: a node of the graph gets the deleted mark, with the consolidation of the graph when the marks are 10 percent of the nodes, and a node of the lists leaves its list.
    pub fn remove(&mut self, ids: &[u64]) {
        let mut out: HashSet<u32, Fixed> = HashSet::default();
        for id in ids {
            self.skipped.remove(id);
            if let Some(node) = self.nodes.remove(id) {
                self.gone[node as usize] = true;
                self.removed += 1;
                if node < self.laid {
                    out.insert(node);
                }
            }
        }
        if out.is_empty() {
            return self.compact_if_sparse();
        }
        match &mut self.layout {
            Layout::None => {}
            Layout::Graph(graph) => {
                for &node in &out {
                    graph.delete(node);
                }
                if graph.needs_consolidation()
                    && let Some(vectors) = &self.vectors
                {
                    graph.consolidate(vectors);
                }
            }
            Layout::Ivf(ivf) => ivf.retain(|node| !out.contains(&node)),
        }
        self.compact_if_sparse();
    }

    /// Makes the vectors again with the nodes that are not gone when they are fewer than a third of the nodes, so that the memory of the removed versions comes back. The layout comes again at the next search.
    fn compact_if_sparse(&mut self) {
        if self.removed <= 2 * self.nodes.len() + 1000 {
            return;
        }
        let Some(old) = self.vectors.take() else { return };
        let mut vectors = Vectors::new(old.dim());
        let mut nodes: HashMap<u64, u32, Fixed> = HashMap::default();
        for node in old.nodes() {
            if !self.gone[node as usize] {
                let id = old.id(node);
                nodes.insert(id, vectors.push(id, old.get(node)));
            }
        }
        self.gone = vec![false; vectors.len()];
        self.vectors = Some(vectors);
        self.nodes = nodes;
        self.removed = 0;
        self.layout = Layout::None;
        self.laid = 0;
        self.built = 0;
    }

    /// Places the nodes that came since the last search: the build of the layout at the first search, and the insert of each later node, as `aminsert` does. The lists of `ivfflat` get new centers when the nodes are twice the nodes of the last k-means and fewer than its sample.
    fn lay(&mut self) {
        let Some(vectors) = &self.vectors else { return };
        let total = vectors.len();
        if total == 0 {
            return;
        }
        let rebuild = match (&self.layout, self.kind) {
            (Layout::None, _) => true,
            (Layout::Ivf(_), Kind::Ivf { lists }) => {
                total >= 2 * self.built.max(1) && self.built < kmeans::sample_size(lists)
            }
            _ => false,
        };
        if rebuild {
            let live = total - self.removed;
            self.layout = match self.kind {
                Kind::Graph { m, ef_construction } => {
                    let params = Params::hnsw(m, ef_construction);
                    let mut graph = Graph::build(vectors, self.metric, params, self.seed);
                    for node in vectors.nodes().filter(|&n| self.gone[n as usize]) {
                        graph.delete(node);
                    }
                    if graph.needs_consolidation() {
                        graph.consolidate(vectors);
                    }
                    Layout::Graph(graph)
                }
                Kind::Ivf { lists } => {
                    let mut ivf = Ivf::build(vectors, lists.max(1), self.metric, self.seed);
                    if self.removed > 0 {
                        ivf.retain(|node| !self.gone[node as usize]);
                    }
                    Layout::Ivf(ivf)
                }
            };
            self.built = live;
        } else {
            let mut visited = Visited::default();
            for node in self.laid..vectors.nodes().end {
                if self.gone[node as usize] {
                    continue;
                }
                match &mut self.layout {
                    Layout::None => {}
                    Layout::Graph(graph) => graph.insert_with(vectors, node, &mut visited),
                    Layout::Ivf(ivf) => ivf.insert(vectors, node),
                }
            }
        }
        self.laid = vectors.nodes().end;
    }

    /// The versions that a scan for `query` reads: the `beam` nearest versions that the search of the graph finds, or the versions of the `probes` lists nearest to `query`. `None` when the index cannot search, as for a query of other dimensions than the vectors.
    pub fn search(&mut self, query: &[f32], beam: usize, probes: usize) -> Option<Vec<u64>> {
        if self.unusable {
            return None;
        }
        let Some(vectors) = &self.vectors else { return Some(Vec::new()) };
        if vectors.dim() != query.len() {
            return None;
        }
        self.lay();
        let vectors = self.vectors.as_ref()?;
        let nodes: Vec<u32> = match &self.layout {
            Layout::None => Vec::new(),
            Layout::Graph(graph) => {
                let mut visited = Visited::default();
                graph
                    .search(vectors, query, beam.max(1), &mut visited)
                    .into_iter()
                    .map(|n| n.node)
                    .collect()
            }
            Layout::Ivf(ivf) => ivf.candidates(query, probes.max(1)),
        };
        Some(nodes.into_iter().filter(|&n| !self.gone[n as usize]).map(|n| vectors.id(n)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{Index, Kind};
    use crate::Metric;

    /// The vector of the point `i` of a line.
    fn point(i: u64) -> Vec<f32> {
        let x = i as f32;
        vec![x, x * 0.5, 1.0]
    }

    fn filled(kind: Kind, n: u64) -> Index {
        let mut index = Index::new(kind, Metric::L2, 7);
        for i in 0..n {
            index.add(i, Some(&point(i)));
        }
        index
    }

    #[test]
    fn graph_finds_the_nearest() {
        let mut index = filled(Kind::Graph { m: 8, ef_construction: 32 }, 500);
        let found = index.search(&point(250), 10, 1).unwrap();
        assert_eq!(found.len(), 10);
        assert!(found.contains(&250));
        assert!(found.iter().all(|&id| id.abs_diff(250) <= 10));
    }

    #[test]
    fn known_and_skipped() {
        let mut index = Index::new(Kind::Ivf { lists: 4 }, Metric::L2, 1);
        index.add(1, None);
        index.add(2, Some(&[1.0, 2.0]));
        index.add(2, Some(&[5.0, 5.0]));
        assert!(index.knows(1) && index.knows(2) && !index.knows(3));
        assert_eq!(index.len(), 1);
        assert_eq!(index.search(&[1.0, 2.0], 1, 1), Some(vec![2]));
        assert_eq!(index.search(&[1.0], 1, 1), None);
        index.add(3, Some(&[1.0]));
        assert!(index.unusable());
        assert_eq!(index.search(&[1.0, 2.0], 1, 1), None);
    }

    #[test]
    fn inserts_after_the_build() {
        let mut index = filled(Kind::Graph { m: 8, ef_construction: 32 }, 100);
        assert!(index.search(&point(0), 5, 1).unwrap().contains(&0));
        for i in 100..300 {
            index.add(i, Some(&point(i)));
        }
        assert!(index.search(&point(280), 5, 1).unwrap().contains(&280));
        let mut lists = filled(Kind::Ivf { lists: 4 }, 40);
        assert_eq!(lists.search(&point(0), 1, 4).unwrap().len(), 40);
        for i in 40..100 {
            lists.add(i, Some(&point(i)));
        }
        assert_eq!(lists.search(&point(0), 1, 4).unwrap().len(), 100);
        let near = lists.search(&point(99), 1, 1).unwrap();
        assert!(near.contains(&99) && near.len() < 100);
    }

    #[test]
    fn removed_versions_leave() {
        for kind in [Kind::Graph { m: 8, ef_construction: 32 }, Kind::Ivf { lists: 3 }] {
            let mut index = filled(kind, 200);
            assert!(index.search(&point(10), 20, 3).unwrap().contains(&10));
            let gone: Vec<u64> = (0..100).collect();
            index.remove(&gone);
            assert!(!index.knows(10));
            assert_eq!(index.len(), 100);
            let found = index.search(&point(10), 20, 3).unwrap();
            assert!(!found.is_empty() && found.iter().all(|&id| id >= 100), "{kind:?}");
            index.add(10, Some(&point(10)));
            assert!(index.search(&point(10), 20, 3).unwrap().contains(&10));
        }
    }

    #[test]
    fn compacts_after_many_removals() {
        let mut index = filled(Kind::Graph { m: 4, ef_construction: 16 }, 3000);
        assert!(index.search(&point(5), 10, 1).is_some());
        let gone: Vec<u64> = (0..2900).collect();
        index.remove(&gone);
        assert_eq!(index.removed, 0);
        assert_eq!(index.vectors.as_ref().map(crate::Vectors::len), Some(100));
        let found = index.search(&point(2950), 10, 1).unwrap();
        assert!(found.contains(&2950));
    }
}
