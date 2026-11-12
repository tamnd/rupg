//! The rows that a scan by an index of `hnsw` or `ivfflat` of pgvector gives, as `hnswgettuple` and `ivfflatgettuple` give them. The index holds no row whose key is null, nor one whose key has a norm of zero when the operator class has a norm function, and the scan gives the nearest rows that its search reaches: `hnsw.ef_search` rows of the graph, or with `hnsw.iterative_scan` up to `hnsw.max_scan_tuples` rows, and for `ivfflat` the rows of the `ivfflat.probes` nearest lists.

use std::collections::BTreeSet;

use rupg_analyze::{Expr, ExprKind, Func, FuncForm, Query};
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
        let mut tuple = vec![NONE; query.relations.len()];
        let mut indexed: Vec<(Option<f64>, usize)> = Vec::new();
        for row in 0..rows.len() {
            tuple[r] = row;
            if eval(&tuple, &scan.key)?.is_null() {
                continue;
            }
            if scan.norm != 0 {
                let call = Expr::new(
                    ExprKind::Func(Func {
                        oid: scan.norm,
                        args: vec![scan.key.clone()],
                        form: FuncForm::Call,
                        variadic: false,
                        retset: false,
                        collation: 0,
                    }),
                    FLOAT8,
                );
                if matches!(eval(&tuple, &call)?, Value::Float8(n) if n == 0.0) {
                    continue;
                }
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
            let probes = number("ivfflat.probes", 1);
            if iterative("ivfflat.iterative_scan") {
                probes.max(number("ivfflat.max_probes", 32_768))
            } else {
                probes
            }
        }
    }
}
