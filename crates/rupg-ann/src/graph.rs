//! The graph layout of `USING hnsw` (spec/13 section 13.4.2): one layer of a proximity graph, built by the Vamana method of DiskANN, with one entry point and one list of neighbors for each node. `m` of `hnsw` sets the out-degree to `2m` and `ef_construction` sets the beam of the build, and a search keeps a beam of `hnsw.ef_search` nodes.
//!
//! An insert searches the graph for the new node, keeps the neighbors that the robust prune of DiskANN keeps, and links each back. A delete marks the node, which stays in the graph as a waypoint so that the graph stays connected, and [`Graph::consolidate`] takes the marked nodes out in the manner of FreshDiskANN once they are 10 percent of the nodes (spec/13 section 13.4.5). A filtered search expands through the nodes that fail the filter to their neighbors, as ACORN-1 does (spec/13 section 13.4.4).

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::metric::Metric;
use crate::rng::Rng;
use crate::vectors::Vectors;
use crate::{Near, Neighbor};

/// The settings of a build.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// The most neighbors of a node.
    pub degree: usize,
    /// The beam of the search of an insert.
    pub beam: usize,
    /// The `alpha` of the robust prune, at least 1. A larger `alpha` keeps more long edges.
    pub alpha: f32,
}

impl Params {
    /// The settings that `m` and `ef_construction` of `hnsw` map to: an out-degree of `2m` and a build beam of `ef_construction`, with an `alpha` of 1.2.
    #[must_use]
    pub fn hnsw(m: usize, ef_construction: usize) -> Params {
        let degree = (2 * m).max(2);
        Params { degree, beam: ef_construction.max(degree), alpha: 1.2 }
    }
}

impl Default for Params {
    /// The defaults of `hnsw`: `m` 16 and `ef_construction` 64.
    fn default() -> Params {
        Params::hnsw(16, 64)
    }
}

/// The nodes that a search visited, which the next search clears in constant time.
#[derive(Clone, Debug, Default)]
pub struct Visited {
    stamps: Vec<u32>,
    now: u32,
}

impl Visited {
    /// Starts a search of a graph of `n` nodes, with no node visited.
    fn start(&mut self, n: usize) {
        if self.stamps.len() < n {
            self.stamps.resize(n, 0);
        }
        self.now = self.now.wrapping_add(1);
        if self.now == 0 {
            self.stamps.fill(0);
            self.now = 1;
        }
    }

    /// Marks `node` visited, and gives true when it was not.
    fn insert(&mut self, node: u32) -> bool {
        let stamp = &mut self.stamps[node as usize];
        if *stamp == self.now {
            false
        } else {
            *stamp = self.now;
            true
        }
    }
}

/// A test of a node for a filtered search.
pub type Filter<'f> = &'f dyn Fn(u32) -> bool;

/// A proximity graph over the nodes of a [`Vectors`].
#[derive(Clone, Debug)]
pub struct Graph {
    metric: Metric,
    params: Params,
    neighbors: Vec<Vec<u32>>,
    entry: Option<u32>,
    /// True for each node that a delete marked or that [`Graph::consolidate`] took out.
    deleted: Vec<bool>,
    /// The nodes that a delete marked since the last consolidation.
    marked: usize,
    /// The nodes in the graph that are not deleted.
    live: usize,
}

impl Graph {
    /// A graph with no nodes.
    #[must_use]
    pub fn new(metric: Metric, params: Params) -> Graph {
        Graph {
            metric,
            params,
            neighbors: Vec::new(),
            entry: None,
            deleted: Vec::new(),
            marked: 0,
            live: 0,
        }
    }

    /// The graph of all the nodes of `vectors`, from the seed `seed`: the medoid as the entry point, a first pass of inserts in a random order with an `alpha` of 1, and a second pass that links each node again with the `alpha` of `params`, as Vamana builds.
    #[must_use]
    pub fn build(vectors: &Vectors, metric: Metric, params: Params, seed: u64) -> Graph {
        let mut graph = Graph::new(metric, params);
        if vectors.is_empty() {
            return graph;
        }
        let mut rng = Rng::new(seed);
        let mut order = rng.permutation(vectors.len());
        let medoid = medoid(vectors, vectors.nodes());
        if let Some(at) = order.iter().position(|&n| n == medoid) {
            order.swap(0, at);
        }
        let mut visited = Visited::default();
        graph.grow(vectors.len());
        for &node in &order {
            graph.add(vectors, node, 1.0, &mut visited);
        }
        if params.alpha > 1.0 {
            for &node in &order {
                graph.link(vectors, node, params.alpha, &mut visited);
            }
        }
        graph
    }

    /// The metric of the graph.
    #[must_use]
    pub fn metric(&self) -> Metric {
        self.metric
    }

    /// The settings of the graph.
    #[must_use]
    pub fn params(&self) -> Params {
        self.params
    }

    /// The entry point of the searches, or `None` for a graph with no live node.
    #[must_use]
    pub fn entry(&self) -> Option<u32> {
        self.entry
    }

    /// The neighbors of `node`.
    #[must_use]
    pub fn neighbors(&self, node: u32) -> &[u32] {
        self.neighbors.get(node as usize).map_or(&[], Vec::as_slice)
    }

    /// The nodes in the graph that are not deleted.
    #[must_use]
    pub fn live(&self) -> usize {
        self.live
    }

    /// The nodes that a delete marked since the last consolidation.
    #[must_use]
    pub fn marked(&self) -> usize {
        self.marked
    }

    /// True when `node` is deleted.
    #[must_use]
    pub fn is_deleted(&self, node: u32) -> bool {
        self.deleted.get(node as usize).copied().unwrap_or(false)
    }

    /// Makes room for the nodes up to `n`.
    fn grow(&mut self, n: usize) {
        if self.neighbors.len() < n {
            self.neighbors.resize_with(n, Vec::new);
            self.deleted.resize(n, false);
        }
    }

    /// Adds the node `node` of `vectors` to the graph, as an insert of a row does.
    pub fn insert(&mut self, vectors: &Vectors, node: u32) {
        self.grow(vectors.len().max(node as usize + 1));
        let mut visited = Visited::default();
        self.add(vectors, node, self.params.alpha, &mut visited);
    }

    /// Adds `node` with the robust prune of `alpha`.
    fn add(&mut self, vectors: &Vectors, node: u32, alpha: f32, visited: &mut Visited) {
        self.live += 1;
        if self.entry.is_none() {
            self.entry = Some(node);
            return;
        }
        self.link(vectors, node, alpha, visited);
    }

    /// Searches for `node`, gives it the neighbors that the robust prune of `alpha` keeps from the nodes that the search expanded and its neighbors so far, and links each of them back to it.
    fn link(&mut self, vectors: &Vectors, node: u32, alpha: f32, visited: &mut Visited) {
        let query = vectors.get(node);
        let (_, mut candidates) = self.beam(vectors, query, self.params.beam, None, visited);
        for &n in &self.neighbors[node as usize] {
            candidates
                .push(Neighbor { distance: self.metric.distance(query, vectors.get(n)), node: n });
        }
        let kept = self.prune(vectors, node, candidates, alpha);
        self.neighbors[node as usize] = kept.clone();
        for n in kept {
            let list = &self.neighbors[n as usize];
            if list.contains(&node) {
                continue;
            }
            if list.len() < self.params.degree {
                self.neighbors[n as usize].push(node);
                continue;
            }
            let base = vectors.get(n);
            let candidates: Vec<Neighbor> = list
                .iter()
                .copied()
                .chain([node])
                .map(|m| Neighbor { distance: self.metric.distance(base, vectors.get(m)), node: m })
                .collect();
            self.neighbors[n as usize] = self.prune(vectors, n, candidates, alpha);
        }
    }

    /// The robust prune of DiskANN: of the candidates of `node`, nearest first, a candidate stays when no kept neighbor is nearer to it by a factor of `alpha` than `node` is, up to the degree. A deleted node is not a new neighbor.
    fn prune(
        &self,
        vectors: &Vectors,
        node: u32,
        mut candidates: Vec<Neighbor>,
        alpha: f32,
    ) -> Vec<u32> {
        candidates.sort_by(Neighbor::order);
        candidates.dedup_by_key(|n| n.node);
        let alpha = if self.metric.scalable() { alpha } else { 1.0 };
        let mut kept: Vec<u32> = Vec::with_capacity(self.params.degree);
        for c in candidates {
            if kept.len() >= self.params.degree {
                break;
            }
            if c.node == node || self.is_deleted(c.node) {
                continue;
            }
            let v = vectors.get(c.node);
            if kept.iter().all(|&k| alpha * self.metric.distance(vectors.get(k), v) > c.distance) {
                kept.push(c.node);
            }
        }
        kept
    }

    /// The nodes that a search expands from `node`: its neighbors, or with a filter the neighbors that pass it and the neighbors that pass it of each neighbor that fails it, as ACORN-1 expands.
    fn hops(&self, node: u32, filter: Option<Filter<'_>>, out: &mut Vec<u32>) {
        out.clear();
        let list = self.neighbors(node);
        match filter {
            None => out.extend_from_slice(list),
            Some(passes) => {
                out.extend(list.iter().copied().filter(|&n| passes(n)));
                for &n in list {
                    if !passes(n) {
                        out.extend(self.neighbors(n).iter().copied().filter(|&m| passes(m)));
                    }
                }
            }
        }
    }

    /// The beam search from the entry point: the `beam` nearest nodes to `query` that are not deleted and pass `filter`, nearest first, and the nodes that the search expanded.
    fn beam(
        &self,
        vectors: &Vectors,
        query: &[f32],
        beam: usize,
        filter: Option<Filter<'_>>,
        visited: &mut Visited,
    ) -> (Vec<Neighbor>, Vec<Neighbor>) {
        let Some(entry) = self.entry else { return (Vec::new(), Vec::new()) };
        let beam = beam.max(1);
        visited.start(self.neighbors.len());
        let passes = |n: u32| !self.is_deleted(n) && filter.is_none_or(|f| f(n));
        let mut candidates: BinaryHeap<Reverse<Near>> = BinaryHeap::new();
        let mut results: BinaryHeap<Near> = BinaryHeap::new();
        let start =
            Neighbor { distance: self.metric.distance(query, vectors.get(entry)), node: entry };
        visited.insert(entry);
        candidates.push(Reverse(Near(start)));
        if passes(entry) {
            results.push(Near(start));
        }
        let mut expanded = Vec::new();
        let mut hops = Vec::new();
        while let Some(Reverse(Near(c))) = candidates.pop() {
            if results.len() >= beam && results.peek().is_some_and(|w| c.distance > w.0.distance) {
                break;
            }
            expanded.push(c);
            self.hops(c.node, filter, &mut hops);
            for &n in &hops {
                if !visited.insert(n) {
                    continue;
                }
                let near =
                    Neighbor { distance: self.metric.distance(query, vectors.get(n)), node: n };
                if results.len() >= beam
                    && results.peek().is_some_and(|w| near.distance >= w.0.distance)
                {
                    continue;
                }
                candidates.push(Reverse(Near(near)));
                if passes(n) {
                    results.push(Near(near));
                    if results.len() > beam {
                        results.pop();
                    }
                }
            }
        }
        let mut out: Vec<Neighbor> = results.into_iter().map(|n| n.0).collect();
        out.sort_by(Neighbor::order);
        (out, expanded)
    }

    /// The `beam` nearest nodes to `query` that are not deleted, nearest first, as a scan with `hnsw.ef_search` of `beam` gives them.
    #[must_use]
    pub fn search(
        &self,
        vectors: &Vectors,
        query: &[f32],
        beam: usize,
        visited: &mut Visited,
    ) -> Vec<Neighbor> {
        self.beam(vectors, query, beam, None, visited).0
    }

    /// The `beam` nearest nodes to `query` that are not deleted and pass `filter`, by the traversal of ACORN-1.
    #[must_use]
    pub fn search_filtered(
        &self,
        vectors: &Vectors,
        query: &[f32],
        beam: usize,
        filter: Filter<'_>,
        visited: &mut Visited,
    ) -> Vec<Neighbor> {
        self.beam(vectors, query, beam, Some(filter), visited).0
    }

    /// Marks `node` deleted, as the delete of its row does. The node stays in the graph as a waypoint.
    pub fn delete(&mut self, node: u32) {
        if let Some(d) = self.deleted.get_mut(node as usize)
            && !*d
        {
            *d = true;
            self.marked += 1;
            self.live -= 1;
        }
    }

    /// True when the marked nodes are 10 percent of the nodes or more, so that [`Graph::consolidate`] should run.
    #[must_use]
    pub fn needs_consolidation(&self) -> bool {
        self.marked > 0 && self.marked * 10 >= self.live + self.marked
    }

    /// Takes the marked nodes out of the graph, in the manner of FreshDiskANN: each node with a marked neighbor gets the robust prune of its other neighbors and the live neighbors of its marked neighbors, and a marked node keeps no neighbors.
    pub fn consolidate(&mut self, vectors: &Vectors) {
        if self.marked == 0 {
            return;
        }
        let mut changed: Vec<(usize, Vec<u32>)> = Vec::new();
        for (p, list) in self.neighbors.iter().enumerate() {
            if self.deleted[p] || !list.iter().any(|&n| self.deleted[n as usize]) {
                continue;
            }
            let base = vectors.get(p as u32);
            let mut candidates = Vec::new();
            for &q in list {
                if self.deleted[q as usize] {
                    candidates.extend(
                        self.neighbors[q as usize]
                            .iter()
                            .copied()
                            .filter(|&r| r as usize != p && !self.deleted[r as usize]),
                    );
                } else {
                    candidates.push(q);
                }
            }
            let candidates: Vec<Neighbor> = candidates
                .into_iter()
                .map(|n| Neighbor { distance: self.metric.distance(base, vectors.get(n)), node: n })
                .collect();
            changed.push((p, self.prune(vectors, p as u32, candidates, self.params.alpha)));
        }
        for (p, list) in changed {
            self.neighbors[p] = list;
        }
        for (p, list) in self.neighbors.iter_mut().enumerate() {
            if self.deleted[p] {
                list.clear();
            }
        }
        self.marked = 0;
        if self.entry.is_some_and(|e| self.deleted[e as usize]) {
            let live = (0..self.neighbors.len() as u32).filter(|&n| !self.deleted[n as usize]);
            self.entry = (self.live > 0).then(|| medoid(vectors, live));
        }
    }
}

/// The node of `nodes` nearest to their mean by the Euclidean distance.
fn medoid(vectors: &Vectors, nodes: impl Iterator<Item = u32> + Clone) -> u32 {
    let dim = vectors.dim();
    let mut mean = vec![0f64; dim];
    let mut count = 0usize;
    for node in nodes.clone() {
        for (m, &x) in mean.iter_mut().zip(vectors.get(node)) {
            *m += f64::from(x);
        }
        count += 1;
    }
    let mean: Vec<f32> = mean.into_iter().map(|m| (m / count.max(1) as f64) as f32).collect();
    let mut best = (f32::INFINITY, 0);
    for node in nodes {
        let d = crate::metric::l2_squared(&mean, vectors.get(node));
        if d < best.0 {
            best = (d, node);
        }
    }
    best.1
}
