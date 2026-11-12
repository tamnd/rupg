//! The checks of the builds and the inserts of the indexes of `hnsw` and `ivfflat` of pgvector: `HnswCheckValue` of a `sparsevec`, which an `hnsw` index holds with at most `HNSW_MAX_NNZ` elements that are not zero, and the notice of `ivfflatbuild` when the rows of the samples of k-means are fewer than the lists.

use std::sync::Arc;

use rupg_analyze::{Expr, ExprKind, Func, FuncForm};
use rupg_catalog::{Catalog, RelKind, Relation};
use rupg_common::{Error, SqlState};
use rupg_plan::AnnMethod;
use rupg_types::Value;

use super::{Connection, explain};
use crate::heap::View;
use rupg_sql::Severity;

use crate::utility::Notice;

/// `HNSW_MAX_NNZ`.
const HNSW_MAX_NNZ: i32 = 1000;

/// `FLOAT8OID`, the type of the norm.
const FLOAT8: u32 = 701;

/// The checks of an index of pgvector.
struct Checks {
    /// The facts of the index.
    method: AnnMethod,
    /// True for an `hnsw` index of `sparsevec`, whose support function 3 is `hnsw_sparsevec_support`.
    sparse: bool,
    /// `IVFFLAT_KMEANS_NORM_PROC` of the operator class, or 0: k-means skips a row whose key has a norm of zero.
    kmeans_norm: u32,
    /// The input type of the operator class.
    intype: u32,
}

/// The function of the support function `number` of the operator class of the key of `index` for its input type, with the input type.
fn support(catalog: &Catalog, index: &Relation, number: i16) -> Option<(u32, u32)> {
    let info = index.index.as_ref()?;
    let class = catalog.opclass(*info.classes.first()?)?;
    let proc = catalog.amprocs().find(|p| {
        p.family == class.family
            && p.number == number
            && p.left == class.intype
            && p.right == class.intype
    })?;
    Some((proc.function, class.intype))
}

/// The checks of `index`, or `None` for an index that is not of pgvector or needs no check.
fn checks(catalog: &Catalog, index: &Relation) -> Option<Checks> {
    let info = index.index.as_ref()?;
    if info.method < rupg_catalog::FIRST_NORMAL_OID {
        return None;
    }
    let ann = explain::ann_index(catalog, index)?;
    let intype = catalog.opclass(*info.classes.first()?).map_or(0, |c| c.intype);
    match ann.method {
        AnnMethod::Hnsw { .. } => {
            let symbol = support(catalog, index, 3)
                .and_then(|(f, _)| catalog.function(f))
                .map(|f| f.src.as_str());
            (symbol == Some("hnsw_sparsevec_support")).then_some(Checks {
                method: ann.method,
                sparse: true,
                kmeans_norm: 0,
                intype,
            })
        }
        AnnMethod::Ivfflat { .. } => Some(Checks {
            method: ann.method,
            sparse: false,
            kmeans_norm: support(catalog, index, 4).map_or(0, |(f, _)| f),
            intype,
        }),
    }
}

/// The elements of a `sparsevec` that are not zero: `nnz`, the second `int32` of its value.
fn nnz(value: &Value) -> i32 {
    match value {
        Value::Bytea(bytes) => {
            bytes.get(4..8).and_then(|b| b.try_into().ok()).map_or(0, i32::from_ne_bytes)
        }
        _ => 0,
    }
}

/// `HnswCheckValue`: the error of a `sparsevec` with more than `HNSW_MAX_NNZ` elements that are not zero.
fn check_nnz(key: &Value) -> Result<(), Error> {
    if nnz(key) > HNSW_MAX_NNZ {
        return Err(Error::new(
            SqlState::INTERNAL_ERROR,
            format!(
                "sparsevec cannot have more than {HNSW_MAX_NNZ} non-zero elements for hnsw index"
            ),
        ));
    }
    Ok(())
}

/// The layout of the index of `rupg-ann` for the index `index` of `hnsw` or `ivfflat`, from its options `m`, `ef_construction` and `lists`, or `None` for another index.
pub(crate) fn vector_kind(catalog: &Catalog, index: &Relation) -> Option<rupg_ann::index::Kind> {
    let ann = explain::ann_index(catalog, index)?;
    let option = |name: &str, default: usize| {
        index
            .options
            .iter()
            .find_map(|text| {
                let (found, value) = text.split_once('=')?;
                (found == name).then(|| value.trim().parse().ok()).flatten()
            })
            .unwrap_or(default)
    };
    let count = |n: i32| usize::try_from(n).unwrap_or(1).max(1);
    Some(match ann.method {
        AnnMethod::Hnsw { m } => rupg_ann::index::Kind::Graph {
            m: count(m),
            ef_construction: option("ef_construction", 64),
        },
        AnnMethod::Ivfflat { lists } => rupg_ann::index::Kind::Ivf { lists: count(lists) },
    })
}

impl Connection {
    /// The keys of the rows `rows` of the table `table` for the index `index`, each the value of its first column, with no key for a row that a partial index does not hold.
    fn ann_keys(
        &self,
        catalog: &Arc<Catalog>,
        index: &Relation,
        rows: &mut dyn Iterator<Item = &Vec<Value>>,
    ) -> Result<Vec<Value>, Error> {
        let Some(info) = &index.index else { return Ok(Vec::new()) };
        let Some(table) = catalog.relation(info.table) else { return Ok(Vec::new()) };
        let columns: Vec<(u32, i32)> = table.columns.iter().map(|c| (c.ty, c.typmod)).collect();
        let derived = rupg_analyze::index_derived(info, info.table, &columns)?;
        let reader = self.reader_without_view(Arc::clone(catalog));
        let mut out = Vec::new();
        for row in rows {
            if let Some(key) = rupg_exec::index_key(&derived, row, &reader)?
                && let Some(first) = key.into_iter().next()
                && !first.is_null()
            {
                out.push(first);
            }
        }
        Ok(out)
    }

    /// `hnswbuild` and `ivfflatbuild` of the indexes of pgvector that the statement built, from the catalog `old` before it to `work`: the error of a key of `sparsevec` with too many elements, and the notice of an `ivfflat` index with fewer rows than lists.
    pub(super) fn ann_builds(
        &self,
        old: &Catalog,
        work: &Arc<Catalog>,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        let built: Vec<(&Relation, Checks)> = work
            .relations()
            .filter(|r| r.kind == RelKind::Index)
            .filter_map(|r| checks(work, r).map(|c| (r, c)))
            .filter(|(r, _)| rupg_analyze::index_built(old, work, r.oid))
            .collect();
        for (index, checks) in built {
            let Some(info) = &index.index else { continue };
            let view = View { xid: self.xid.get(), ..self.view() };
            let rows = self.heap().rows(info.table, &view);
            let keys = self.ann_keys(work, index, &mut rows.iter())?;
            match checks.method {
                AnnMethod::Hnsw { .. } => {
                    for key in &keys {
                        check_nnz(key)?;
                    }
                }
                AnnMethod::Ivfflat { lists } => {
                    let mut samples = keys.len();
                    if checks.kmeans_norm != 0 {
                        let reader = self.reader_without_view(Arc::clone(work));
                        samples = 0;
                        for key in keys {
                            let call = Expr::new(
                                ExprKind::Func(Func {
                                    oid: checks.kmeans_norm,
                                    args: vec![Expr::constant(key, checks.intype, -1, None)],
                                    form: FuncForm::Call,
                                    variadic: false,
                                    retset: false,
                                    collation: 0,
                                }),
                                FLOAT8,
                            );
                            if !matches!(rupg_exec::evaluate(&call, &reader)?, Value::Float8(n) if n == 0.0)
                            {
                                samples += 1;
                            }
                        }
                    }
                    if samples < usize::try_from(lists).unwrap_or(0) {
                        let error = Error::new(
                            SqlState::SUCCESSFUL_COMPLETION,
                            "ivfflat index created with little data",
                        )
                        .with_detail("This will cause low recall.")
                        .with_hint("Drop the index until the table has more data.");
                        notices.push(Notice { severity: Severity::Notice, error });
                    }
                }
            }
        }
        Ok(())
    }

    /// `hnswinsert` of the rows `rows` that a statement writes to the table `table`: the error of a key of `sparsevec` with too many elements for an `hnsw` index of the table.
    pub(super) fn ann_inserts<'r>(
        &self,
        table: u32,
        rows: impl Iterator<Item = &'r Vec<Value>> + Clone,
    ) -> Result<(), Error> {
        let catalog = self.catalog();
        let sparse: Vec<&Relation> = catalog
            .indexes_of(table)
            .filter(|r| r.index.as_ref().is_some_and(|i| i.ready))
            .filter(|r| checks(&catalog, r).is_some_and(|c| c.sparse))
            .collect();
        for index in sparse {
            for key in self.ann_keys(&catalog, index, &mut rows.clone())? {
                check_nnz(&key)?;
            }
        }
        Ok(())
    }
}
