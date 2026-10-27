//! `REFRESH MATERIALIZED VIEW`, as `ExecRefreshMatView` and `RefreshMatViewByOid` of `matview.c` run it. The rows of the materialized view go as `TRUNCATE` removes them, and the caller then fills it with the rows of its query, as `refresh_matview_datafill` writes them.

use rupg_catalog::{IndexInfo, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::RefreshMatViewStmt;

use super::comment::not_owner;
use super::drop::{owner_word, quote_identifier};
use super::{Definer, Found, not_yet, relname};

/// `is_usable_unique_index`: a unique index with no deferred check, no predicate and no expression, which `REFRESH ... CONCURRENTLY` can match the old rows and the new rows with.
fn usable_unique(index: &IndexInfo) -> bool {
    index.unique
        && index.immediate
        && index.valid
        && index.predicate.is_none()
        && index.keys.iter().take(usize::try_from(index.key_count).unwrap_or(0)).all(|&k| k > 0)
}

impl Definer<'_, '_> {
    /// `ExecRefreshMatView` up to `refresh_matview_datafill`. The result is the OID of the materialized view when the caller fills it with the rows of its query, or `None` for `WITH NO DATA`. `CONCURRENTLY` makes the rows again as the statement without it does.
    pub(super) fn refresh_matview(&mut self, stmt: &RefreshMatViewStmt) -> Result<Option<u32>> {
        let rv = stmt.relation.as_deref().cloned().unwrap_or_default();
        let name = relname(&rv);
        // `RangeVarGetRelidExtended` has no parse state here, so its errors have no position.
        let found = self.lookup_relation(&rv).map_err(|e| e.with_position(None))?;
        let kind = self.kind_of(found);
        let Found::User(oid) = found else {
            return Err(not_yet("REFRESH MATERIALIZED VIEW of a system catalog", None));
        };
        let Some(rel) = self.catalog.relation(oid).cloned() else {
            return Err(Error::internal(format!("cache lookup failed for relation {oid}")));
        };
        // `RangeVarCallbackMaintainsTable`: rupg has no `MAINTAIN` privilege, so only the owner can refresh.
        if !self.is_superuser() && !self.owns(rel.owner) {
            return Err(not_owner(owner_word(kind), name));
        }
        if rel.kind != RelKind::MatView {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("\"{}\" is not a materialized view", rel.name),
            ));
        }
        let populated = rel.view.as_ref().is_some_and(|v| v.populated);
        if stmt.concurrent && !populated {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "CONCURRENTLY cannot be used when the materialized view is not populated",
            ));
        }
        if stmt.concurrent && stmt.skipData {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "CONCURRENTLY and WITH NO DATA options cannot be used together",
            ));
        }
        if stmt.concurrent {
            let found = self
                .catalog
                .relations()
                .filter_map(|r| r.index.as_ref())
                .any(|index| index.table == oid && usable_unique(index));
            if !found {
                let schema = self.schema_name(rel.namespace);
                return Err(Error::new(
                    SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!(
                        "cannot refresh materialized view \"{}.{}\" concurrently",
                        quote_identifier(&schema),
                        quote_identifier(&rel.name)
                    ),
                )
                .with_hint(
                    "Create a unique index with no WHERE clause on one or more columns of the materialized view.",
                ));
            }
        }
        self.truncated.push(oid);
        self.catalog.set_populated(oid, !stmt.skipData)?;
        Ok((!stmt.skipData).then_some(oid))
    }
}
