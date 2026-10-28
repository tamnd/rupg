//! The options of relations, `reloptions`: `WITH (...)` of `CREATE TABLE` and `CREATE INDEX`, and `SET (...)` and `RESET (...)` of `ALTER TABLE`, `ALTER INDEX` and `ALTER VIEW`, as `DefineRelation`, `DefineIndex` and `ATExecSetRelOptions` run them. An option in the namespace `toast` goes to the TOAST table of a table. When the table has no TOAST table, the option is checked and then dropped.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::Node;

use super::alter::Target;
use super::reloptions::{self, Opt};
use super::{Definer, relkind_detail};

/// `HEAP_RELOPT_NAMESPACES`: the namespaces that the options of a table can have.
const HEAP_NAMESPACES: &[&str] = &["toast"];

/// The OID of the access method `hash`.
const HASH: u32 = 405;
/// The OID of the access method `gist`.
const GIST: u32 = 783;
/// The OID of the access method `gin`.
const GIN: u32 = 2742;
/// The OID of the access method `spgist`.
const SPGIST: u32 = 4000;
/// The OID of the access method `brin`.
const BRIN: u32 = 3580;

/// `amoptions`: the options that an index of the access method accepts.
fn index_options(method: u32) -> &'static [Opt] {
    match method {
        HASH => reloptions::HASH,
        GIST => reloptions::GIST,
        GIN => reloptions::GIN,
        SPGIST => reloptions::SPGIST,
        BRIN => reloptions::BRIN,
        _ => reloptions::BTREE,
    }
}

impl Definer<'_, '_> {
    /// `DefineRelation`: the options of `WITH` with no namespace, checked as the options of a table.
    pub(super) fn create_table_options(&self, defs: &[Option<Node>]) -> Result<Vec<String>> {
        let options = reloptions::transform_create(defs, None, HEAP_NAMESPACES)?;
        reloptions::validate(&options, reloptions::HEAP, false)?;
        Ok(options)
    }

    /// `ProcessUtilitySlow` after `DefineRelation`: the options of `WITH` in the namespace `toast` are checked as the options of a TOAST table, and they go to the TOAST table when the table has one.
    pub(super) fn create_toast_options(&mut self, table: u32, defs: &[Option<Node>]) -> Result<()> {
        let options = reloptions::transform_create(defs, Some("toast"), HEAP_NAMESPACES)?;
        reloptions::validate(&options, reloptions::HEAP, true)?;
        let toast = self.catalog.relation(table).map_or(0, |r| r.toast);
        if toast != 0 && !options.is_empty() {
            self.catalog.set_options(toast, options)?;
        }
        Ok(())
    }

    /// `DefineIndex`: the options of `WITH`, checked by the access method. An index accepts no namespace.
    pub(super) fn create_index_options(
        &self,
        method: u32,
        defs: &[Option<Node>],
    ) -> Result<Vec<String>> {
        let options = reloptions::transform(&[], defs, None, &[], false)?;
        reloptions::validate(&options, index_options(method), false)?;
        Ok(options)
    }

    /// `ATExecSetRelOptions`: `SET (...)` or, when `reset` is true, `RESET (...)` of the options of the relation, then of its TOAST table.
    pub(super) fn set_rel_options(
        &mut self,
        target: &Target,
        defs: &[Option<Node>],
        reset: bool,
    ) -> Result<()> {
        if defs.is_empty() {
            return Ok(());
        }
        let Some(rel) = self.catalog.relation(target.oid) else { return Ok(()) };
        let toast = rel.toast;
        let method = rel.index.as_ref().map_or(0, |i| i.method);
        let options = reloptions::transform(&rel.options, defs, None, HEAP_NAMESPACES, reset)?;
        match target.kind {
            b'r' | b'm' => reloptions::validate(&options, reloptions::HEAP, false)?,
            b'p' if !options.is_empty() => {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    "cannot specify storage parameters for a partitioned table",
                )
                .with_hint("Specify storage parameters for its leaf partitions instead."));
            }
            b'p' => {}
            b'v' => reloptions::validate(&options, reloptions::VIEW, false)?,
            b'i' | b'I' => reloptions::validate(&options, index_options(method), false)?,
            kind => {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("cannot set options for relation \"{}\"", target.name),
                )
                .with_detail(relkind_detail(kind)));
            }
        }
        // A view with a check option must be automatically updatable.
        if target.kind == b'v' && options.iter().any(|o| o.starts_with("check_option=")) {
            let (query, _) = self
                .an
                .stored_query_perms(target.oid)?
                .ok_or_else(|| Error::internal("a view with no query"))?;
            self.an.auto_updatable(&query, true).map_err(super::view::check_option_error)?;
        }
        self.catalog.set_options(target.oid, options)?;
        if let Some(rel) = self.catalog.relation(toast) {
            let options =
                reloptions::transform(&rel.options, defs, Some("toast"), HEAP_NAMESPACES, reset)?;
            reloptions::validate(&options, reloptions::HEAP, true)?;
            self.catalog.set_options(toast, options)?;
        }
        Ok(())
    }
}
