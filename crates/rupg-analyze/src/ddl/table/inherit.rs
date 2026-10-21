//! `CREATE TABLE ... INHERITS`, as `DefineRelation` and `MergeAttributes` run it: the checks of the parents, the columns that the new table takes from them, and the columns of the statement that merge with those columns.

use rupg_catalog::{RelKind, Relation};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::{Node, TypeName};

use super::super::alter_column::attribute_storage;
use super::super::partition_attach::without_locations;
use super::super::{Definer, Found, Message, not_yet, relkind_detail};
use super::{NewColumn, Plan, RawDefault};
use crate::typename::place;
use crate::types;

/// `storage_name`: the name of a value of `attstorage`.
fn storage_name(storage: u8) -> &'static str {
    match storage {
        b'p' => "PLAIN",
        b'm' => "MAIN",
        b'x' => "EXTENDED",
        b'e' => "EXTERNAL",
        _ => "???",
    }
}

/// The name of a value of `attcompression`.
fn compression_name(compression: u8) -> &'static str {
    match compression {
        b'p' => "pglz",
        b'l' => "lz4",
        _ => "",
    }
}

/// `get_collation_name`.
fn collation_name(collation: u32) -> &'static str {
    builtin::named_by_oid(Named::Collation, collation).map_or("???", |n| n.name)
}

/// The error of two definitions of a column that do not agree, with the two values in the detail.
fn conflict(state: SqlState, message: String, first: &str, second: &str) -> Error {
    Error::new(state, message).with_detail(format!("{first} versus {second}"))
}

impl Definer<'_, '_> {
    /// The loop of `DefineRelation` over the parents of `INHERITS`: the OID of each parent, in order. A parent cannot come twice.
    pub(super) fn inherit_parents(&self, inh: &[Option<Node>]) -> Result<Vec<u32>> {
        let mut parents = Vec::new();
        for node in inh.iter().flatten() {
            let Node::RangeVar(rv) = node else { continue };
            let Found::User(oid) = self.lookup_relation(rv)? else {
                return Err(not_yet("INHERITS of a system catalog", place(rv.location)));
            };
            if parents.contains(&oid) {
                let name = self.catalog.relation(oid).map(|r| r.name.clone()).unwrap_or_default();
                return Err(Error::new(
                    SqlState::DUPLICATE_TABLE,
                    format!("relation \"{name}\" would be inherited from more than once"),
                ));
            }
            parents.push(oid);
        }
        Ok(parents)
    }

    /// The checks of `MergeAttributes` on one parent of `INHERITS`, and then the parent. `namespace` is the schema of the new table.
    fn inherit_parent(&self, parent: u32, namespace: u32) -> Result<Relation> {
        let rel =
            self.catalog.relation(parent).cloned().ok_or_else(|| {
                Error::internal(format!("cache lookup failed for relation {parent}"))
            })?;
        let name = &rel.name;
        let kind = self.kind_of(Found::User(parent));
        // `table_open` refuses indexes and composite types first.
        if matches!(kind, b'i' | b'I' | b'c') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot open relation \"{name}\""),
            )
            .with_detail(relkind_detail(kind)));
        }
        let wrong = |message: String| Error::new(SqlState::WRONG_OBJECT_TYPE, message);
        if rel.kind == RelKind::Partitioned {
            return Err(wrong(format!("cannot inherit from partitioned table \"{name}\"")));
        }
        if rel.partition_of.is_some() {
            return Err(wrong(format!("cannot inherit from partition \"{name}\"")));
        }
        if !matches!(kind, b'r' | b'f') {
            return Err(wrong(format!(
                "inherited relation \"{name}\" is not a table or foreign table"
            )));
        }
        let temp = self.catalog.is_temp_schema(rel.namespace);
        if temp && !self.catalog.is_temp_schema(namespace) {
            return Err(wrong(format!("cannot inherit from temporary relation \"{name}\"")));
        }
        if temp && rel.namespace != self.an.temp {
            return Err(wrong("cannot inherit from temporary relation of another session".into()));
        }
        if !self.is_superuser() && rel.owner != self.user {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of table {name}"),
            ));
        }
        Ok(rel)
    }

    /// `MergeAttributes` for `INHERITS`. The columns of the parents come first, in the order of the parents and of their columns, and the columns of two parents with the same name are one column, as `MergeInheritedAttribute` merges them. A column of the statement with the name of an inherited column merges with it, as `MergeChildAttribute` merges it, and the other columns of the statement come after the inherited columns. `namespace` is the schema of the new table.
    pub(super) fn merge_inherited_columns(
        &mut self,
        plan: &mut Plan,
        parents: &[u32],
        namespace: u32,
    ) -> Result<()> {
        let local = std::mem::take(&mut plan.columns);
        for (i, column) in local.iter().enumerate() {
            if local[..i].iter().any(|c| c.name == column.name) {
                return Err(Error::new(
                    SqlState::DUPLICATE_COLUMN,
                    format!("column \"{}\" specified more than once", column.name),
                ));
            }
        }
        let mut columns: Vec<NewColumn> = Vec::new();
        // The columns whose parents give different defaults. A default of the statement must replace them.
        let mut bogus: Vec<String> = Vec::new();
        for &parent in parents {
            let rel = self.inherit_parent(parent, namespace)?;
            for (i, c) in rel.columns.iter().enumerate().filter(|(_, c)| !c.dropped) {
                let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
                let default = c
                    .has_default
                    .then(|| self.catalog.default_of(parent, attnum))
                    .flatten()
                    .map(|d| d.expr.clone());
                let Some(prev) = columns.iter_mut().find(|n| n.name == c.name) else {
                    let mut inherited = c.clone();
                    // A child of `INHERITS` does not take the identity of the column.
                    inherited.identity = 0;
                    inherited.local = false;
                    inherited.inhcount = 1;
                    columns.push(NewColumn {
                        name: c.name.clone(),
                        type_name: TypeName::default(),
                        collate: None,
                        collation: c.collation,
                        is_not_null: false,
                        default: default.map(RawDefault::Cooked),
                        identity: 0,
                        storage: None,
                        compression: None,
                        inherited: Some(inherited),
                    });
                    continue;
                };
                self.notice(
                    SqlState::SUCCESSFUL_COMPLETION,
                    format!("merging multiple inherited definitions of column \"{}\"", c.name),
                );
                let Some(inh) = prev.inherited.as_mut() else { continue };
                if (inh.ty, inh.typmod) != (c.ty, c.typmod) {
                    return Err(conflict(
                        SqlState::DATATYPE_MISMATCH,
                        format!("inherited column \"{}\" has a type conflict", c.name),
                        &types::with_typmod(inh.ty, inh.typmod),
                        &types::with_typmod(c.ty, c.typmod),
                    ));
                }
                if inh.collation != c.collation {
                    return Err(conflict(
                        SqlState::COLLATION_MISMATCH,
                        format!("inherited column \"{}\" has a collation conflict", c.name),
                        &format!("\"{}\"", collation_name(inh.collation)),
                        &format!("\"{}\"", collation_name(c.collation)),
                    ));
                }
                if inh.storage != c.storage {
                    return Err(conflict(
                        SqlState::DATATYPE_MISMATCH,
                        format!("inherited column \"{}\" has a storage parameter conflict", c.name),
                        storage_name(inh.storage),
                        storage_name(c.storage),
                    ));
                }
                if inh.compression == 0 {
                    inh.compression = c.compression;
                } else if c.compression != 0 && inh.compression != c.compression {
                    return Err(conflict(
                        SqlState::DATATYPE_MISMATCH,
                        format!("column \"{}\" has a compression method conflict", c.name),
                        compression_name(inh.compression),
                        compression_name(c.compression),
                    ));
                }
                inh.inhcount = inh.inhcount.saturating_add(1);
                match (&prev.default, default) {
                    (None, Some(text)) => prev.default = Some(RawDefault::Cooked(text)),
                    (Some(RawDefault::Cooked(old)), Some(text))
                        if without_locations(Some(old)) != without_locations(Some(&text))
                            && !bogus.contains(&c.name) =>
                    {
                        bogus.push(c.name.clone());
                    }
                    _ => {}
                }
            }
        }
        // `newcol_attno`: the place of the column of the statement among the columns of the statement that do not merge.
        let mut position = 0;
        let mut places = Vec::with_capacity(local.len());
        for column in local {
            position += 1;
            let Some(at) = columns.iter().position(|c| c.name == column.name) else {
                places.push(columns.len());
                columns.push(column);
                continue;
            };
            let name = column.name.clone();
            let message = if at + 1 == position {
                Error::new(
                    SqlState::SUCCESSFUL_COMPLETION,
                    format!("merging column \"{name}\" with inherited definition"),
                )
            } else {
                Error::new(
                    SqlState::SUCCESSFUL_COMPLETION,
                    format!("moving and merging column \"{name}\" with inherited definition"),
                )
                .with_detail("User-specified column moved to the position of the inherited column.")
            };
            self.flush();
            self.messages.push(Message::Notice(message));
            let new = self.catalog_column(&column)?;
            let target = &mut columns[at];
            let Some(inh) = target.inherited.as_mut() else { continue };
            if (inh.ty, inh.typmod) != (new.ty, new.typmod) {
                return Err(conflict(
                    SqlState::DATATYPE_MISMATCH,
                    format!("column \"{name}\" has a type conflict"),
                    &types::with_typmod(inh.ty, inh.typmod),
                    &types::with_typmod(new.ty, new.typmod),
                ));
            }
            if inh.collation != new.collation {
                return Err(conflict(
                    SqlState::COLLATION_MISMATCH,
                    format!("column \"{name}\" has a collation conflict"),
                    &format!("\"{}\"", collation_name(inh.collation)),
                    &format!("\"{}\"", collation_name(new.collation)),
                ));
            }
            // A child of `INHERITS` takes the identity of the column of the statement.
            inh.identity = new.identity;
            if let Some(storage) = &column.storage {
                let storage = attribute_storage(new.ty, storage)?;
                if inh.storage != storage {
                    return Err(conflict(
                        SqlState::DATATYPE_MISMATCH,
                        format!("column \"{name}\" has a storage parameter conflict"),
                        storage_name(inh.storage),
                        storage_name(storage),
                    ));
                }
            }
            if column.compression.is_some() {
                if inh.compression == 0 {
                    inh.compression = new.compression;
                } else if inh.compression != new.compression {
                    return Err(conflict(
                        SqlState::DATATYPE_MISMATCH,
                        format!("column \"{name}\" has a compression method conflict"),
                        compression_name(inh.compression),
                        compression_name(new.compression),
                    ));
                }
            }
            inh.local = true;
            target.is_not_null |= column.is_not_null;
            if column.default.is_some() {
                target.default = column.default;
                bogus.retain(|b| *b != name);
            }
            position -= 1;
            places.push(at);
        }
        if let Some(name) = columns.iter().map(|c| &c.name).find(|n| bogus.contains(n)) {
            return Err(Error::new(
                SqlState::INVALID_COLUMN_DEFINITION,
                format!("column \"{name}\" inherits conflicting default values"),
            )
            .with_hint("To resolve the conflict, specify a default explicitly."));
        }
        // The `serial` columns of the statement have new places.
        for serial in &mut plan.serials {
            serial.column = places.get(serial.column).copied().unwrap_or(serial.column);
        }
        plan.columns = columns;
        Ok(())
    }
}
