//! The recall tests of spec/13 section 13.8 at the size of a unit test: each layout against the exact search, and each filtered search with only rows that pass the filter.

use crate::filter::{Strategy, Thresholds, post_filter, pre_filter, strategy};
use crate::graph::{Graph, Params, Visited};
use crate::ivf::Ivf;
use crate::kmeans;
use crate::rabitq::{EPSILON0, EXACT_EPSILON, Quantizer, rank};
use crate::rng::Rng;
use crate::{Metric, Neighbor, Vectors, exact, recall};

/// `n` vectors of `dim` dimensions around `clusters` centers, from `seed`.
fn clustered(n: usize, dim: usize, clusters: usize, seed: u64) -> Vectors {
    let mut rng = Rng::new(seed);
    let centers: Vec<Vec<f32>> =
        (0..clusters).map(|_| (0..dim).map(|_| (rng.gaussian() * 4.0) as f32).collect()).collect();
    let mut vectors = Vectors::new(dim);
    for i in 0..n {
        let v: Vec<f32> =
            centers[i % clusters].iter().map(|&c| c + rng.gaussian() as f32).collect();
        vectors.push(i as u64 * 7, &v);
    }
    vectors
}

/// The queries of the data of [`clustered`] for `vectors`: 40 more vectors of the same distribution, from the same seed.
fn queries(vectors: &Vectors, clusters: usize, seed: u64) -> Vec<Vec<f32>> {
    let n = vectors.len();
    let more = clustered(n + 40, vectors.dim(), clusters, seed);
    (n as u32..more.len() as u32).map(|node| more.get(node).to_vec()).collect()
}

/// The mean recall at `k` of `search` against the exact search of the nodes that pass `keep`.
fn mean_recall(
    (vectors, clusters, seed): (&Vectors, usize, u64),
    metric: Metric,
    k: usize,
    keep: &dyn Fn(u32) -> bool,
    search: &mut dyn FnMut(&[f32]) -> Vec<Neighbor>,
) -> f64 {
    let queries = queries(vectors, clusters, seed);
    let mut total = 0.0;
    for q in &queries {
        let want = exact(vectors, metric, q, vectors.nodes().filter(|&n| keep(n)), k);
        let got = search(q);
        assert!(got.iter().all(|n| keep(n.node)), "a search gave a node that it must not give");
        let mut got = got;
        got.truncate(k);
        total += recall(&got, &want);
    }
    total / queries.len() as f64
}

#[test]
fn exact_is_sorted_and_recall_is_one() {
    let vectors = clustered(300, 8, 4, 1);
    let q = vectors.get(5).to_vec();
    let found = exact(&vectors, Metric::L2, &q, vectors.nodes(), 10);
    assert_eq!(found.len(), 10);
    assert_eq!(found[0].node, 5);
    assert!(found.windows(2).all(|w| w[0].distance <= w[1].distance));
    assert!((recall(&found, &found) - 1.0).abs() < f64::EPSILON);
    assert_eq!(vectors.id(5), 35);
}

#[test]
fn kmeans_is_deterministic_and_quick_with_few_rows() {
    let vectors = clustered(500, 8, 5, 2);
    let a = kmeans::centers(&vectors, 5, Metric::L2, 7);
    let b = kmeans::centers(&vectors, 5, Metric::L2, 7);
    assert_eq!(a, b);
    assert_eq!(a.len(), 5 * 8);
    // With no more rows than lists, each row is a center.
    let few = clustered(3, 8, 3, 3);
    let centers = kmeans::centers(&few, 5, Metric::L2, 7);
    assert_eq!(centers.len(), 5 * 8);
    for node in few.nodes() {
        assert!(centers.as_chunks::<8>().0.iter().any(|c| c.as_slice() == few.get(node)));
    }
    // The centers of the sphere have a norm of 1.
    let spherical = kmeans::centers(&vectors, 5, Metric::Cosine, 7);
    for c in spherical.as_chunks::<8>().0 {
        assert!((crate::metric::dot(c, c) - 1.0).abs() < 1e-4);
    }
    assert_eq!(kmeans::sample_size(10), 10_000);
    assert_eq!(kmeans::sample_size(1000), 50_000);
}

#[test]
fn ivf_with_all_probes_is_exact() {
    let vectors = clustered(2000, 16, 16, 4);
    let ivf = Ivf::build(&vectors, 16, Metric::L2, 11);
    assert_eq!(ivf.len(), 16);
    assert_eq!((0..ivf.len()).map(|l| ivf.list(l).len()).sum::<usize>(), 2000);
    let r = mean_recall((&vectors, 16, 4), Metric::L2, 10, &|_| true, &mut |q| {
        ivf.search(&vectors, q, 16, 10)
    });
    assert!((r - 1.0).abs() < f64::EPSILON, "recall {r}");
}

#[test]
fn ivf_with_few_probes_has_recall() {
    let vectors = clustered(2000, 16, 16, 4);
    let ivf = Ivf::build(&vectors, 16, Metric::L2, 11);
    let one = mean_recall((&vectors, 16, 4), Metric::L2, 10, &|_| true, &mut |q| {
        ivf.search(&vectors, q, 1, 10)
    });
    let three = mean_recall((&vectors, 16, 4), Metric::L2, 10, &|_| true, &mut |q| {
        ivf.search(&vectors, q, 3, 10)
    });
    assert!(one >= 0.8, "recall {one} with 1 probe");
    assert!(three >= one && three >= 0.95, "recall {three} with 3 probes");
    // The nearest list of a center is its own list.
    assert_eq!(ivf.nearest_lists(ivf.center(4), 1), vec![4]);
}

#[test]
fn ivf_inserts_after_the_build_and_removes() {
    let all = clustered(1000, 8, 8, 5);
    let mut first = Vectors::new(8);
    for node in all.nodes().take(500) {
        first.push(all.id(node), all.get(node));
    }
    let mut ivf = Ivf::build(&first, 8, Metric::Cosine, 3);
    let mut vectors = first;
    for node in all.nodes().skip(500) {
        let n = vectors.push(all.id(node), all.get(node));
        ivf.insert(&vectors, n);
    }
    let mut seen: Vec<u32> = (0..ivf.len()).flat_map(|l| ivf.list(l).to_vec()).collect();
    seen.sort_unstable();
    assert_eq!(seen, vectors.nodes().collect::<Vec<_>>());
    ivf.remove(700);
    assert!((0..ivf.len()).all(|l| !ivf.list(l).contains(&700)));
    let r = mean_recall((&vectors, 8, 5), Metric::Cosine, 10, &|n| n != 700, &mut |q| {
        ivf.search(&vectors, q, 8, 10)
    });
    assert!((r - 1.0).abs() < f64::EPSILON, "recall {r}");
}

#[test]
fn graph_recall_l2() {
    let vectors = clustered(3000, 24, 16, 6);
    let graph = Graph::build(&vectors, Metric::L2, Params::hnsw(12, 64), 1);
    assert_eq!(graph.live(), 3000);
    assert!(vectors.nodes().all(|n| graph.neighbors(n).len() <= 24));
    let mut visited = Visited::default();
    let r = mean_recall((&vectors, 16, 6), Metric::L2, 10, &|_| true, &mut |q| {
        graph.search(&vectors, q, 40, &mut visited)
    });
    assert!(r >= 0.95, "recall {r}");
}

#[test]
fn graph_recall_cosine_and_inner_product() {
    let vectors = clustered(2000, 16, 16, 7);
    for (metric, least) in [(Metric::Cosine, 0.95), (Metric::InnerProduct, 0.8), (Metric::L1, 0.9)]
    {
        let graph = Graph::build(&vectors, metric, Params::default(), 2);
        let mut visited = Visited::default();
        let r = mean_recall((&vectors, 16, 7), metric, 10, &|_| true, &mut |q| {
            graph.search(&vectors, q, 40, &mut visited)
        });
        assert!(r >= least, "recall {r} of {metric:?}");
    }
}

#[test]
fn graph_of_few_nodes_reaches_each() {
    let vectors = clustered(20, 4, 2, 8);
    let graph = Graph::build(&vectors, Metric::L2, Params::default(), 3);
    let mut visited = Visited::default();
    let found = graph.search(&vectors, vectors.get(0), 40, &mut visited);
    assert_eq!(found.len(), 20);
    let empty = Graph::build(&Vectors::new(4), Metric::L2, Params::default(), 3);
    assert!(empty.search(&vectors, vectors.get(0), 40, &mut visited).is_empty());
}

#[test]
fn graph_inserts_after_the_build() {
    let all = clustered(2000, 16, 16, 9);
    let mut vectors = Vectors::new(16);
    for node in all.nodes().take(1000) {
        vectors.push(all.id(node), all.get(node));
    }
    let mut graph = Graph::build(&vectors, Metric::L2, Params::default(), 4);
    for node in all.nodes().skip(1000) {
        let n = vectors.push(all.id(node), all.get(node));
        graph.insert(&vectors, n);
    }
    assert_eq!(graph.live(), 2000);
    let mut visited = Visited::default();
    let r = mean_recall((&vectors, 16, 9), Metric::L2, 10, &|_| true, &mut |q| {
        graph.search(&vectors, q, 40, &mut visited)
    });
    assert!(r >= 0.95, "recall {r}");
}

#[test]
fn graph_deletes_and_consolidates() {
    let vectors = clustered(2000, 16, 16, 10);
    let mut graph = Graph::build(&vectors, Metric::L2, Params::default(), 5);
    let gone = |n: u32| n % 7 == 3;
    for node in vectors.nodes().filter(|&n| gone(n)) {
        graph.delete(node);
    }
    graph.delete(3);
    assert_eq!(graph.marked(), 286);
    assert!(graph.needs_consolidation());
    let mut visited = Visited::default();
    let before = mean_recall((&vectors, 16, 10), Metric::L2, 10, &|n| !gone(n), &mut |q| {
        graph.search(&vectors, q, 40, &mut visited)
    });
    assert!(before >= 0.95, "recall {before} with marks");
    graph.consolidate(&vectors);
    assert_eq!(graph.marked(), 0);
    assert!(!graph.needs_consolidation());
    for node in vectors.nodes() {
        assert!(
            graph.neighbors(node).iter().all(|&n| !gone(n)),
            "node {node} keeps a deleted neighbor"
        );
        if gone(node) {
            assert!(graph.neighbors(node).is_empty());
        }
    }
    assert!(graph.entry().is_some_and(|e| !gone(e)));
    let after = mean_recall((&vectors, 16, 10), Metric::L2, 10, &|n| !gone(n), &mut |q| {
        graph.search(&vectors, q, 40, &mut visited)
    });
    assert!(after >= 0.95, "recall {after} after the consolidation");
}

#[test]
fn filtered_searches_give_only_passing_rows() {
    let vectors = clustered(3000, 16, 16, 11);
    let graph = Graph::build(&vectors, Metric::L2, Params::default(), 6);
    let mut visited = Visited::default();
    // 10 percent pass: the traversal of ACORN-1.
    let tenth = |n: u32| n % 10 == 4;
    let r = mean_recall((&vectors, 16, 11), Metric::L2, 10, &tenth, &mut |q| {
        graph.search_filtered(&vectors, q, 40, &tenth, &mut visited)
    });
    assert!(r >= 0.9, "recall {r} of the traversal");
    // 60 percent pass: the post-filter search.
    let most = |n: u32| n % 5 < 3;
    let r = mean_recall((&vectors, 16, 11), Metric::L2, 10, &most, &mut |q| {
        post_filter(&graph, &vectors, q, 10, 40, 20_000, &most)
    });
    assert!(r >= 0.95, "recall {r} of the post-filter");
    // 1 percent pass: the pre-filter scan, which is exact.
    let few = |n: u32| n.is_multiple_of(100);
    let quantizer = Quantizer::fit(&vectors, Metric::L2, 1).expect("a quantizer");
    let codes = quantizer.encode_all(&vectors);
    let r = mean_recall((&vectors, 16, 11), Metric::L2, 10, &few, &mut |q| {
        pre_filter(
            &vectors,
            Metric::L2,
            q,
            vectors.nodes().filter(|&n| few(n)),
            10,
            Some((&quantizer, &codes)),
        )
    });
    assert!((r - 1.0).abs() < f64::EPSILON, "recall {r} of the pre-filter");
    // A post-filter search whose filter passes too few rows stops at the most tuples of the scan.
    let none = |_: u32| false;
    assert!(post_filter(&graph, &vectors, vectors.get(0), 10, 40, 200, &none).is_empty());
}

#[test]
fn strategies_follow_the_selectivity() {
    let t = Thresholds::default();
    assert_eq!(strategy(10_000.0, 0.9, &t), Strategy::PreFilter);
    assert_eq!(strategy(10_000_000.0, 0.004, &t), Strategy::PreFilter);
    assert_eq!(strategy(10_000_000.0, 0.005, &t), Strategy::PreFilter);
    assert_eq!(strategy(100_000_000.0, 0.005, &t), Strategy::PreFilter);
    assert_eq!(strategy(10_000_000.0, 0.05, &t), Strategy::Traversal);
    assert_eq!(strategy(10_000_000.0, 0.5, &t), Strategy::Traversal);
    assert_eq!(strategy(10_000_000.0, 0.6, &t), Strategy::PostFilter);
}

#[test]
fn rabitq_codes_are_small_and_rank_exactly() {
    let probe = Quantizer::new(Metric::L2, vec![0.0; 1536], 1).expect("a quantizer");
    assert_eq!(probe.code_bytes(), 192);
    assert!(Quantizer::new(Metric::L1, vec![0.0; 8], 1).is_none());
    let vectors = clustered(3000, 48, 16, 12);
    for metric in [Metric::L2, Metric::Cosine, Metric::InnerProduct] {
        let quantizer = Quantizer::fit(&vectors, metric, 2).expect("a quantizer");
        let codes = quantizer.encode_all(&vectors);
        assert_eq!(codes.len(), 3000);
        for (epsilon, least, most) in [(EPSILON0, 0.97, 3000 / 8), (EXACT_EPSILON, 1.0, 3000 / 3)] {
            let mut reads = 0;
            let mut runs = 0;
            let r = mean_recall((&vectors, 16, 12), metric, 10, &|_| true, &mut |q| {
                let (found, read) =
                    rank(&quantizer, &codes, &vectors, q, vectors.nodes(), 10, epsilon);
                reads += read;
                runs += 1;
                found
            });
            assert!(r >= least, "recall {r} of {metric:?} at {epsilon}");
            let mean = reads / runs;
            assert!(
                mean < most,
                "the rerank read {mean} of 3000 vectors for {metric:?} at {epsilon}"
            );
        }
    }
}

#[test]
fn rabitq_bounds_hold() {
    let vectors = clustered(1000, 100, 8, 13);
    let quantizer = Quantizer::fit(&vectors, Metric::L2, 3).expect("a quantizer");
    let codes = quantizer.encode_all(&vectors);
    let mut held = 0;
    let mut total = 0;
    for q in queries(&vectors, 8, 13).iter().take(10) {
        let query = quantizer.query(q);
        for node in vectors.nodes() {
            let e = quantizer.estimate(&codes, node, &query);
            let d = Metric::L2.distance(q, vectors.get(node));
            total += 1;
            if (e.distance - d).abs() <= e.error * 1.001 + 1e-3 {
                held += 1;
            }
        }
    }
    assert!(held * 100 >= total * 95, "the bound held for {held} of {total}");
    // The code of the center itself is exact.
    let center = Quantizer::new(Metric::L2, vec![1.0; 6], 4).expect("a quantizer");
    let mut one = Vectors::new(6);
    one.push(0, &[1.0; 6]);
    let codes = center.encode_all(&one);
    let e = center.estimate(&codes, 0, &center.query(&[3.0; 6]));
    assert!((e.distance - 24.0).abs() < 1e-3 && e.error.abs() < 1e-3, "{e:?}");
}
