//! `ALTER TYPE ... ADD VALUE` and `ALTER TYPE ... RENAME VALUE` of an enum type, as `AlterEnum`, `AddEnumLabel` and `RenameEnumLabel` run them.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{AlterEnumStmt, TypeName};

use super::Definer;
use super::comment::not_owner;
use super::enumtype::MAX_LABEL;
use crate::typename::type_name_text;

impl Definer<'_, '_> {
    /// `AlterEnum`: the type of `typenameTypeId`, the checks of `checkEnumOwner`, and then the new label or the new name of a label.
    pub(super) fn alter_enum(&mut self, stmt: &AlterEnumStmt) -> Result<()> {
        let name = TypeName { names: stmt.typeName.clone(), ..TypeName::default() };
        let ty = self.an.lookup_type_name(&name)?.ok_or_else(|| {
            Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("type \"{}\" does not exist", type_name_text(&name)),
            )
        })?;
        let found = self.catalog.type_by_oid(ty);
        let Some((owner, labels)) = found.and_then(|t| Some((t.owner, t.labels.as_ref()?))) else {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("{} is not an enum", self.type_text(ty)),
            ));
        };
        let labels: Vec<String> = labels.iter().map(|l| l.label.clone()).collect();
        if !self.is_superuser() && owner != self.user {
            return Err(not_owner("type", &self.type_text(ty)));
        }
        let new = stmt.newVal.as_deref().unwrap_or_default();
        check_label(new)?;
        let not_existing = |label: &str| {
            Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("\"{label}\" is not an existing enum label"),
            )
        };
        let exists =
            Error::new(SqlState::DUPLICATE_OBJECT, format!("enum label \"{new}\" already exists"));
        if let Some(old) = stmt.oldVal.as_deref() {
            let at = labels.iter().position(|l| l == old).ok_or_else(|| not_existing(old))?;
            if labels.iter().any(|l| l == new) {
                return Err(exists);
            }
            return self.catalog.rename_enum_label(ty, at, new.to_string());
        }
        if labels.iter().any(|l| l == new) {
            if !stmt.skipIfNewValExists {
                return Err(exists);
            }
            self.notice(SqlState::DUPLICATE_OBJECT, format!("{}, skipping", exists.message()));
            return Ok(());
        }
        let neighbor = match stmt.newValNeighbor.as_deref() {
            Some(near) => {
                let at = labels.iter().position(|l| l == near).ok_or_else(|| not_existing(near))?;
                Some((at, stmt.newValIsAfter))
            }
            None => None,
        };
        self.catalog.add_enum_label(ty, new.to_string(), neighbor)
    }
}

/// The check of the length of a new label, as a label is a `name`.
fn check_label(label: &str) -> Result<()> {
    if label.len() > MAX_LABEL {
        return Err(Error::new(SqlState::INVALID_NAME, format!("invalid enum label \"{label}\""))
            .with_detail(format!("Labels must be {MAX_LABEL} bytes or less.")));
    }
    Ok(())
}
