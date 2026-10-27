//! The checks of a row type that a column holds: `CheckAttributeType` for a composite type that would hold itself, and `find_composite_type_dependencies` for a change of the columns of a row type that a stored value holds.

use rupg_catalog::{Catalog, RangePart, RelKind, Relation};
use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;

use super::Definer;
use super::alter::Target;
use crate::expr::{Expr, ExprKind};

/// The depth of domains, arrays and composite types that the checks follow.
const MAX_DEPTH: usize = 32;

/// The relation of the composite type `ty` of the catalog, also through domains, or `None` for another type.
pub(super) fn row_relation(catalog: &Catalog, ty: u32) -> Option<&Relation> {
    let mut ty = ty;
    for _ in 0..MAX_DEPTH {
        let found = catalog.type_by_oid(ty)?;
        match &found.domain {
            Some(domain) => ty = domain.base,
            None => return catalog.relation(found.relation),
        }
    }
    None
}

/// Moves the fields after the place `at` of the composite types `types` by one place to the front: a field selection of a later field, and the field at `at` of a `ROW` or a constant of one of the types. True when the expression changes.
fn move_fields(expr: &mut Expr, types: &[u32], at: usize) -> bool {
    let ty = expr.ty;
    let mut changed = true;
    match &mut expr.kind {
        ExprKind::FieldSelect(arg, field) if *field > at && types.contains(&arg.ty) => {
            *field -= 1;
        }
        ExprKind::Row(args, _) if at < args.len() && types.contains(&ty) => {
            args.remove(at);
        }
        ExprKind::Const(Value::Record(record))
            if at < record.values.len() && types.contains(&ty) =>
        {
            record.values.remove(at);
            record.types.remove(at);
        }
        _ => changed = false,
    }
    for child in expr.children_mut() {
        changed |= move_fields(child, types, at);
    }
    changed
}

impl Definer<'_, '_> {
    /// The containing row types of `CheckAttributeType`: a column of the type `ty` cannot be in the relation of the row type `row_type` when `ty` is the row type, or a domain, an array or a composite type that holds it.
    pub(super) fn check_member_of_itself(&self, ty: u32, row_type: u32) -> Result<()> {
        if row_type != 0 && self.holds_type(ty, row_type, true, 0) {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!(
                    "composite type {} cannot be made a member of itself",
                    self.type_text(row_type)
                ),
            ));
        }
        Ok(())
    }

    /// `find_composite_type_dependencies` for the row type of the relation of `target`: a column of a table that holds the row type, also through a domain, an array or another composite type, stops a change of the type of a column, as the stored values keep their fields.
    pub(super) fn check_row_type_users(&self, target: &Target) -> Result<()> {
        let row_type = self.catalog.relation(target.oid).map_or(0, |r| r.row_type);
        let Some((table, column)) = self.row_type_user(row_type, 0) else { return Ok(()) };
        let message = if target.kind == b'c' {
            format!(
                "cannot alter type \"{}\" because column \"{table}.{column}\" uses it",
                target.name
            )
        } else {
            format!(
                "cannot alter table \"{}\" because column \"{table}.{column}\" uses its row type",
                target.name
            )
        };
        Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, message))
    }

    /// The stored expressions after `ALTER TYPE ... DROP ATTRIBUTE` or `ALTER TABLE ... DROP COLUMN` of the field at the place `at` of the row type of `relation`. PostgreSQL finds a field by its attribute number, but a field selection of rupg has the place of the field among the fields that are not dropped, so the fields after `at` move by one place.
    pub(super) fn drop_field(&mut self, relation: u32, at: usize) -> Result<()> {
        let catalog = &*self.catalog;
        let types: Vec<u32> = catalog
            .types()
            .filter(|t| row_relation(catalog, t.oid).is_some_and(|r| r.oid == relation))
            .map(|t| t.oid)
            .collect();
        if types.is_empty() {
            return Ok(());
        }
        self.catalog.change_stored_exprs(&mut |text, list| {
            if list {
                let mut exprs = crate::node::read_list(text)?;
                let mut changed = false;
                for expr in &mut exprs {
                    changed |= move_fields(expr, &types, at);
                }
                return if changed { crate::node::write_list(&exprs).map(Some) } else { Ok(None) };
            }
            let mut expr = crate::node::read(text)?;
            if move_fields(&mut expr, &types, at) {
                crate::node::write(&expr).map(Some)
            } else {
                Ok(None)
            }
        })
    }

    /// The first column of a table that holds the type `ty`, as the names of the table and of the column. A column of a relation with no rows that holds the type makes its row type one that holds the type too.
    fn row_type_user(&self, ty: u32, depth: usize) -> Option<(String, String)> {
        if ty == 0 || depth > MAX_DEPTH {
            return None;
        }
        for rel in self.catalog.relations() {
            for column in rel.columns.iter().filter(|c| !c.dropped) {
                if !self.holds_type(column.ty, ty, false, 0) {
                    continue;
                }
                if matches!(rel.kind, RelKind::Table | RelKind::Partitioned) {
                    return Some((rel.name.clone(), column.name.clone()));
                }
                if let Some(found) = self.row_type_user(rel.row_type, depth + 1) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// True when the type `ty` is the type `target`, or a domain, an array, a range or a multirange of a type that holds it. With `fields`, the columns of a composite type count too.
    fn holds_type(&self, ty: u32, target: u32, fields: bool, depth: usize) -> bool {
        if ty == target {
            return true;
        }
        let Some(found) = self.catalog.type_by_oid(ty).filter(|_| depth <= MAX_DEPTH) else {
            return false;
        };
        if let Some(domain) = &found.domain {
            return self.holds_type(domain.base, target, fields, depth + 1);
        }
        if found.element != 0 {
            return self.holds_type(found.element, target, fields, depth + 1);
        }
        let inner = match &found.range {
            Some(RangePart::Range(_)) => crate::types::range_subtype(ty),
            Some(RangePart::Multirange(_)) => crate::types::multirange_range(ty),
            None => 0,
        };
        if inner != 0 {
            return self.holds_type(inner, target, fields, depth + 1);
        }
        fields
            && self.catalog.relation(found.relation).is_some_and(|rel| {
                rel.columns
                    .iter()
                    .filter(|c| !c.dropped)
                    .any(|c| self.holds_type(c.ty, target, fields, depth + 1))
            })
    }
}
