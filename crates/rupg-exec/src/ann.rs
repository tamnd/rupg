//! The rows that a scan by an index of `hnsw` or `ivfflat` of pgvector gives, as `hnswgettuple` and `ivfflatgettuple` give them. The index holds no row whose key is null, nor one whose key has a norm of zero when the operator class has a norm function, and the scan gives the nearest rows that its search reaches: `hnsw.ef_search` rows of the graph, or with `hnsw.iterative_scan` up to `hnsw.max_scan_tuples` rows, and for `ivfflat` the rows of the `ivfflat.probes` nearest lists.

use std::collections::BTreeSet;

use rupg_analyze::{Expr, ExprKind, Func, FuncForm, Query};
use rupg_ann::Metric;
use rupg_common::Result;
use rupg_plan::{AnnMethod, AnnScan};
use rupg_types::Value;

use crate::Session;
use crate::scan::{NONE, Tables};

/// `FLOAT8OID`, the type of the norm.
const FLOAT8: u32 = 701;

/// True when the table `table` has an index of an access method of the catalog, as the ones of an extension, so that its scan may be one of [`keep`].
pub(crate) fn indexed(session: &dyn Session, table: u32) -> bool {
    let Some(catalog) = session.catalog() else { return false };
    catalog
        .indexes_of(table)
        .any(|rel| rel.index.as_ref().is_some_and(|i| i.method >= rupg_catalog::FIRST_NORMAL_OID))
}

/// The tuples of `tuples` whose rows the scans `scans` of `query` give, with the rows that `eval` finds for an expression of a tuple.
pub(crate) fn keep(
    query: &Query,
    tables: &Tables,
    scans: &[AnnScan],
    session: &dyn Session,
    eval: &mut dyn FnMut(&[usize], &Expr) -> Result<Value>,
    tuples: Vec<Vec<usize>>,
) -> Result<Vec<Vec<usize>>> {
    let mut tuples = tuples;
    for scan in scans {
        let r = scan.relation;
        let Some(relation) = query.relations.get(r) else { continue };
        if relation.oid != scan.table
            || tables.oids.get(r) != Some(&scan.table)
            || tables.needs.get(r).is_some_and(|n| !n.is_empty())
        {
            continue;
        }
        let Some(rows) = tables.rows.get(r) else { continue };
        if let Some(kept) = shared(query, tables, scan, session, eval)? {
            tuples.retain(|t| t[r] == NONE || kept.contains(&t[r]));
            continue;
        }
        let mut tuple = vec![NONE; query.relations.len()];
        let mut indexed: Vec<(Option<f64>, usize)> = Vec::new();
        for row in 0..rows.len() {
            tuple[r] = row;
            if eval(&tuple, &scan.key)?.is_null() {
                continue;
            }
            if scan.norm != 0
                && matches!(eval(&tuple, &norm_call(scan))?, Value::Float8(n) if n == 0.0)
            {
                continue;
            }
            let distance = match eval(&tuple, &scan.distance)? {
                Value::Float8(d) if !d.is_nan() => Some(d),
                _ => None,
            };
            indexed.push((distance, row));
        }
        let count = indexed.len();
        let limit = limit(scan.method, count, session);
        // A null query has no distance, so each row is as near as another and the scan gives the first ones.
        indexed.sort_by(|a, b| match (a.0, b.0) {
            (Some(x), Some(y)) => x.total_cmp(&y).then(a.1.cmp(&b.1)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.1.cmp(&b.1),
        });
        let kept: BTreeSet<usize> = indexed.iter().take(limit).map(|&(_, row)| row).collect();
        tuples.retain(|t| t[r] == NONE || kept.contains(&t[r]));
    }
    Ok(tuples)
}

/// The metric of the distance `distance` of a scan, `<->`, `<#>`, `<=>` or `<+>` of `vector` or of `halfvec`, with true for `halfvec`, and the value that the scan orders by, or `None` for another distance.
fn metric_of<'e>(session: &dyn Session, distance: &'e Expr) -> Option<(Metric, bool, &'e Expr)> {
    let ExprKind::Func(f) = &distance.kind else { return None };
    let [_, value] = f.args.as_slice() else { return None };
    let symbol = session.catalog()?.function(f.oid)?.src.as_str();
    let (half, name) = match symbol.strip_prefix("halfvec_") {
        Some(name) => (true, name),
        None => (false, symbol),
    };
    let metric = match name {
        "l2_distance" | "l2_squared_distance" | "vector_l2_squared_distance" => Metric::L2,
        "negative_inner_product" | "vector_negative_inner_product" => Metric::InnerProduct,
        "cosine_distance" => Metric::Cosine,
        "l1_distance" => Metric::L1,
        _ => return None,
    };
    Some((metric, half, value))
}

/// The rows of the scan `scan` that the search of the index of `rupg-ann` that the session shares finds, or `None` when the scan reads each row instead: with no shared index, a value that is not a constant of the scan, a row of another table, or no more rows than [`limit`] gives, so that the search would find each of them.
///
/// The index adds each version that it does not know, with the key of its row, or with no vector for a row that it does not hold, and then searches: the `limit` nearest versions that the graph of `hnsw` finds, or the versions of the `ivfflat.probes` lists nearest to the value. The versions that the snapshot of the scan does not see are not rows of the scan, as the heap gives no row for them.
fn shared(
    query: &Query,
    tables: &Tables,
    scan: &AnnScan,
    session: &dyn Session,
    eval: &mut dyn FnMut(&[usize], &Expr) -> Result<Value>,
) -> Result<Option<BTreeSet<usize>>> {
    let r = scan.relation;
    let Some(rows) = tables.rows.get(r) else { return Ok(None) };
    let Some((metric, half, value)) = metric_of(session, &scan.distance) else { return Ok(None) };
    if !crate::reduce::relations_of(value).is_empty() {
        return Ok(None);
    }
    let mut versions = Vec::with_capacity(rows.len());
    for row in 0..rows.len() {
        let Some(id) = tables.version(r, row).filter(|_| tables.leaf(r, row) == scan.table) else {
            return Ok(None);
        };
        versions.push(id);
    }
    let Some(index) = session.vector_index(scan.index, metric) else { return Ok(None) };
    let lock = || index.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let unknown: Vec<usize> = {
        let index = lock();
        (0..rows.len()).filter(|&row| !index.knows(versions[row])).collect()
    };
    // The keys come with no lock of the index, as a key can call a function that scans the table again.
    let mut tuple = vec![NONE; query.relations.len()];
    let mut added = Vec::with_capacity(unknown.len());
    for row in unknown {
        tuple[r] = row;
        let vector = match key_of(scan, &tuple, eval)? {
            Some(Value::Bytea(data)) => match rupg_ann::vectors::decode(&data, half) {
                Some(vector) => Some(vector),
                None => return Ok(None),
            },
            Some(_) => return Ok(None),
            None => None,
        };
        added.push((versions[row], vector));
    }
    let point = match eval(&vec![NONE; query.relations.len()], value)? {
        Value::Bytea(data) => rupg_ann::vectors::decode(&data, half),
        _ => None,
    };
    let mut index = lock();
    for (id, vector) in &added {
        index.add(*id, vector.as_deref());
    }
    let Some(point) = point.filter(|_| !index.unusable()) else { return Ok(None) };
    let held: Vec<usize> = (0..rows.len()).filter(|&row| index.holds(versions[row])).collect();
    let count = held.len();
    let (beam, probes) = match scan.method {
        AnnMethod::Hnsw { .. } => {
            let limit = limit(scan.method, count, session);
            if count <= limit {
                return Ok(Some(held.into_iter().collect()));
            }
            (limit, 0)
        }
        AnnMethod::Ivfflat { lists } => {
            let lists = usize::try_from(lists).unwrap_or(1);
            if lists <= 1 {
                return Ok(Some(held.into_iter().collect()));
            }
            if count <= lists {
                return Ok(None);
            }
            (0, probes(session))
        }
    };
    let Some(found) = index.search(&point, beam, probes) else { return Ok(None) };
    drop(index);
    let found: BTreeSet<u64> = found.into_iter().collect();
    Ok(Some(held.into_iter().filter(|&row| found.contains(&versions[row])).collect()))
}

/// The key of the scan `scan` for the row of `tuple`, or `None` for a key that the index does not hold: a null, or a key whose norm is zero when the operator class has a norm function.
fn key_of(
    scan: &AnnScan,
    tuple: &[usize],
    eval: &mut dyn FnMut(&[usize], &Expr) -> Result<Value>,
) -> Result<Option<Value>> {
    let key = eval(tuple, &scan.key)?;
    if key.is_null() {
        return Ok(None);
    }
    if scan.norm != 0 && matches!(eval(tuple, &norm_call(scan))?, Value::Float8(n) if n == 0.0) {
        return Ok(None);
    }
    Ok(Some(key))
}

/// The call of the norm function of the scan `scan` on its key.
fn norm_call(scan: &AnnScan) -> Expr {
    Expr::new(
        ExprKind::Func(Func {
            oid: scan.norm,
            args: vec![scan.key.clone()],
            form: FuncForm::Call,
            variadic: false,
            retset: false,
            collation: 0,
        }),
        FLOAT8,
    )
}

/// The lists that a scan by an index of `ivfflat` reads: `ivfflat.probes`, or with `ivfflat.iterative_scan` up to `ivfflat.max_probes`.
fn probes(session: &dyn Session) -> usize {
    let number = |name: &str, default: usize| {
        session.setting(name).and_then(|s| s.trim().parse().ok()).unwrap_or(default)
    };
    let probes = number("ivfflat.probes", 1);
    if session.setting("ivfflat.iterative_scan").is_some_and(|s| s != "off") {
        probes.max(number("ivfflat.max_probes", 32_768))
    } else {
        probes
    }
}

/// The rows that the scan by an index of `method` with `count` rows gives.
fn limit(method: AnnMethod, count: usize, session: &dyn Session) -> usize {
    let number = |name: &str, default: usize| {
        session.setting(name).and_then(|s| s.trim().parse().ok()).unwrap_or(default)
    };
    let iterative = |name: &str| session.setting(name).is_some_and(|s| s != "off");
    match method {
        AnnMethod::Hnsw { .. } => {
            if iterative("hnsw.iterative_scan") {
                number("hnsw.max_scan_tuples", 20_000)
            } else {
                number("hnsw.ef_search", 40)
            }
        }
        AnnMethod::Ivfflat { lists } => {
            let lists = usize::try_from(lists).unwrap_or(1);
            // `QuickCenters`: with no more rows than lists, each row is the center of its list. With more rows, the centers of k-means put the rows in lists that a scan of all the lists reads.
            if lists <= 1 || count > lists {
                return count;
            }
            probes(session)
        }
    }
}
