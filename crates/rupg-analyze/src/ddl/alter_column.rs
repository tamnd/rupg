//! The subcommands of `ALTER TABLE` that change the facts of one column for the planner and for the storage: `SET STATISTICS`, `SET STORAGE`, `SET COMPRESSION`, and `SET` and `RESET` of the options, as `ATExecSetStatistics`, `ATExecSetStorage`, `ATExecSetCompression` and `ATExecSetOptions` run them.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::Node;

use super::Definer;
use super::alter::Target;
use super::reloptions;
use super::table::SYSTEM_COLUMNS;
use crate::types;

/// `MAX_STATISTICS_TARGET`.
const MAX_STATISTICS_TARGET: i32 = 10000;

/// A subcommand of `AT_PASS_MISC` that changes the facts of a column.
pub(super) enum ColumnCmd {
    /// `SET STATISTICS` of the column with the name or, for an index, with the number, and the target, or `None` for `DEFAULT`.
    Statistics(Option<String>, i16, Option<i32>),
    /// `SET STORAGE` of the column, with the name of the storage.
    Storage(String, String),
    /// `SET COMPRESSION` of the column, with the name of the method.
    Compression(String, String),
    /// `SET` of the options of the column, or `RESET` when the flag is true.
    Options(String, Vec<Option<Node>>, bool),
}

/// `GetAttributeStorage`: the `attstorage` code of the name of a storage for a column of the type.
pub(super) fn attribute_storage(ty: u32, name: &str) -> Result<u8> {
    let own = types::row(ty).map_or(b'x', |t| t.storage);
    let storage = match name.to_ascii_lowercase().as_str() {
        "plain" => b'p',
        "external" => b'e',
        "extended" => b'x',
        "main" => b'm',
        "default" => own,
        _ => {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("invalid storage type \"{name}\""),
            ));
        }
    };
    if storage != b'p' && own == b'p' {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("column data type {} can only have storage PLAIN", types::name(ty)),
        ));
    }
    Ok(storage)
}

/// `GetAttributeCompression`: the `attcompression` code of the name of a method for a column of the type, or 0 for `default`.
pub(super) fn attribute_compression(ty: u32, name: &str) -> Result<u8> {
    if name == "default" {
        return Ok(0);
    }
    if types::row(ty).map_or(b'x', |t| t.storage) == b'p' {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("column data type {} does not support compression", types::name(ty)),
        ));
    }
    match name {
        "pglz" => Ok(b'p'),
        "lz4" => Ok(b'l'),
        _ => Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("invalid compression method \"{name}\""),
        )),
    }
}

impl Definer<'_, '_> {
    /// The subcommand of `AT_PASS_MISC` for the column.
    pub(super) fn alter_column_facts(&mut self, target: &Target, cmd: &ColumnCmd) -> Result<()> {
        match cmd {
            ColumnCmd::Statistics(name, number, value) => {
                self.set_statistics(target, name.as_deref(), *number, *value)
            }
            ColumnCmd::Storage(column, name) => {
                let (number, ty) = self.alter_column_number(target, column)?;
                let storage = attribute_storage(ty, name)?;
                self.catalog.set_storage(target.oid, number, storage)
            }
            ColumnCmd::Compression(column, name) => {
                let (number, ty) = self.alter_column_number(target, column)?;
                let compression = attribute_compression(ty, name)?;
                self.catalog.set_compression(target.oid, number, compression)
            }
            ColumnCmd::Options(column, defs, reset) => {
                let (number, _) = self.alter_column_number(target, column)?;
                let old = self
                    .catalog
                    .relation(target.oid)
                    .and_then(|r| r.column(number))
                    .map(|c| c.options.clone())
                    .unwrap_or_default();
                let options = reloptions::transform(&old, defs, None, &[], *reset)?;
                reloptions::validate(&options, reloptions::ATTRIBUTE)?;
                self.catalog.set_column_options(target.oid, number, options)
            }
        }
    }

    /// The number and the type of the column with the name, for a subcommand that cannot change a system column.
    fn alter_column_number(&self, target: &Target, column: &str) -> Result<(i16, u32)> {
        let rel = self.catalog.relation(target.oid);
        let found = rel.and_then(|r| r.column_number(column).map(|n| (n, r.column(n))));
        match found {
            Some((number, Some(c))) => Ok((number, c.ty)),
            _ if SYSTEM_COLUMNS.contains(&column) && !is_index(target) => Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cannot alter system column \"{column}\""),
            )),
            _ => Err(Error::new(
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{column}\" of relation \"{}\" does not exist", target.name),
            )),
        }
    }

    /// `ATExecSetStatistics`: the statistics target of a column of a table, or of an expression column of an index. Only an index column can have a number in place of a name.
    fn set_statistics(
        &mut self,
        target: &Target,
        name: Option<&str>,
        number: i16,
        value: Option<i32>,
    ) -> Result<()> {
        let index = is_index(target);
        if !index && name.is_none() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot refer to non-index column by number",
            ));
        }
        let value = value.filter(|&v| v != -1);
        if let Some(v) = value
            && v < 0
        {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("statistics target {v} is too low"),
            ));
        }
        if value.is_some_and(|v| v > MAX_STATISTICS_TARGET) {
            self.warning(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("lowering statistics target to {MAX_STATISTICS_TARGET}"),
            );
        }
        let value = value.map(|v| i16::try_from(v.min(MAX_STATISTICS_TARGET)).unwrap_or(i16::MAX));
        let number = match name {
            Some(column) => self.alter_column_number(target, column)?.0,
            None => {
                let rel = self.catalog.relation(target.oid);
                if rel.and_then(|r| r.column(number)).is_none() {
                    return Err(Error::new(
                        SqlState::UNDEFINED_COLUMN,
                        format!(
                            "column number {number} of relation \"{}\" does not exist",
                            target.name
                        ),
                    ));
                }
                number
            }
        };
        if index && let Some(rel) = self.catalog.relation(target.oid) {
            let column = rel.column(number).map_or("", |c| c.name.as_str());
            let info = rel.index.as_ref();
            if info.is_some_and(|i| number > i.key_count) {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!(
                        "cannot alter statistics on included column \"{column}\" of index \"{}\"",
                        target.name
                    ),
                ));
            }
            let key =
                info.and_then(|i| usize::try_from(number - 1).ok().and_then(|n| i.keys.get(n)));
            if key.is_some_and(|&k| k != 0) {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!(
                        "cannot alter statistics on non-expression column \"{column}\" of index \"{}\"",
                        target.name
                    ),
                )
                .with_hint("Alter statistics on table column instead."));
            }
        }
        self.catalog.set_stat_target(target.oid, number, value)
    }
}

/// True for an index or a partitioned index.
fn is_index(target: &Target) -> bool {
    matches!(target.kind, b'i' | b'I')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_names() {
        assert_eq!(attribute_storage(rupg_types::oid::TEXT, "MAIN").unwrap(), b'm');
        assert_eq!(attribute_storage(rupg_types::oid::INT4, "default").unwrap(), b'p');
        assert!(attribute_storage(rupg_types::oid::INT4, "main").is_err());
        assert!(attribute_storage(rupg_types::oid::TEXT, "bogus").is_err());
        assert_eq!(attribute_compression(rupg_types::oid::TEXT, "default").unwrap(), 0);
        assert!(attribute_compression(rupg_types::oid::INT4, "bogus").is_err());
    }
}
