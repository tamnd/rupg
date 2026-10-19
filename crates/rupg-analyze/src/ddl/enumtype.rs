//! `CREATE TYPE ... AS ENUM`, as `DefineEnum` and `EnumValuesCreate` run it.

use rupg_catalog::NewEnum;
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::CreateEnumStmt;

use super::Definer;
use crate::typename::names;

/// The longest label in bytes, as a label is a `name`.
pub(super) const MAX_LABEL: usize = 63;

impl Definer<'_, '_> {
    /// `DefineEnum`: the enum type, its labels and its array type.
    pub(super) fn create_enum(&mut self, stmt: &CreateEnumStmt) -> Result<()> {
        let parts = names(&stmt.typeName);
        let (schema, name) = self.an.split_name(&parts, None)?;
        let namespace = match schema {
            Some(namespace) => namespace,
            None => self.an.env.search_path().first().copied().ok_or_else(|| {
                Error::new(SqlState::UNDEFINED_SCHEMA, "no schema has been selected to create in")
            })?,
        };
        self.check_create_in(namespace, None)?;
        self.check_type_name(namespace, name)?;
        let labels = names(&stmt.vals);
        for (i, label) in labels.iter().enumerate() {
            if label.len() > MAX_LABEL {
                return Err(Error::new(
                    SqlState::INVALID_NAME,
                    format!("invalid enum label \"{label}\""),
                )
                .with_detail(format!("Labels must be {MAX_LABEL} bytes or less.")));
            }
            if labels[..i].contains(label) {
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!("enum label \"{label}\" used more than once"),
                ));
            }
        }
        let new = NewEnum {
            namespace,
            name: name.to_string(),
            owner: self.user,
            labels: labels.into_iter().map(str::to_string).collect(),
        };
        self.catalog.create_enum(new)?;
        Ok(())
    }
}
