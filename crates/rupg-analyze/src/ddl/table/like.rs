//! `CREATE TABLE ... (LIKE ...)`: `transformTableLikeClause` gives the table the columns and the not-null constraints of another relation in the place of the clause, and `expandTableLikeClause` gives it the check constraints, the indexes and the comments of that relation after the table exists.

use rupg_catalog::{ConKind, ObjRef, PG_CLASS, PG_CONSTRAINT};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{ConstrType, Constraint, DefElem, Node, TableLikeClause, TypeName};

use super::super::{Definer, Found, column_numbers, not_yet, relkind_detail};
use super::{NewColumn, Plan, RawDefault, Serial};
use crate::coerce::AtOpt;
use crate::typename::place;

/// `CREATE_TABLE_LIKE_COMMENTS`.
const COMMENTS: u32 = 1 << 0;
/// `CREATE_TABLE_LIKE_COMPRESSION`.
const COMPRESSION: u32 = 1 << 1;
/// `CREATE_TABLE_LIKE_CONSTRAINTS`.
const CONSTRAINTS: u32 = 1 << 2;
/// `CREATE_TABLE_LIKE_DEFAULTS`.
const DEFAULTS: u32 = 1 << 3;
/// `CREATE_TABLE_LIKE_GENERATED`.
const GENERATED: u32 = 1 << 4;
/// `CREATE_TABLE_LIKE_IDENTITY`.
const IDENTITY: u32 = 1 << 5;
/// `CREATE_TABLE_LIKE_INDEXES`.
const INDEXES: u32 = 1 << 6;
/// `CREATE_TABLE_LIKE_STATISTICS`.
const STATISTICS: u32 = 1 << 7;
/// `CREATE_TABLE_LIKE_STORAGE`.
const STORAGE: u32 = 1 << 8;

/// A `LIKE` clause after `transformTableLikeClause`, for `expandTableLikeClause`.
pub(super) struct Like {
    /// The OID of the relation of the clause.
    relation: u32,
    /// The `CREATE_TABLE_LIKE_*` options.
    options: u32,
}

/// The name of an `attstorage` code, as `storage_name` gives it.
fn storage_name(storage: u8) -> Option<&'static str> {
    match storage {
        b'p' => Some("plain"),
        b'e' => Some("external"),
        b'x' => Some("extended"),
        b'm' => Some("main"),
        _ => None,
    }
}

/// The name of an `attcompression` code, as `GetCompressionMethodName` gives it.
fn compression_name(compression: u8) -> Option<&'static str> {
    match compression {
        b'p' => Some("pglz"),
        b'l' => Some("lz4"),
        _ => None,
    }
}

/// The error of `expandTableLikeClause` for a whole-row reference, which the new table cannot keep when its row type changes later.
fn whole_row(detail: String) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot convert whole-row table reference")
        .with_detail(detail)
}

/// A `DefElem` of `sequence_options` with a number, as `makeFloat` gives it.
fn number_option(name: &str, value: i64) -> Option<Node> {
    let def = DefElem {
        defname: Some(name.into()),
        arg: Some(Node::Float(value.to_string().as_str().into())),
        location: -1,
        ..DefElem::default()
    };
    Some(Node::DefElem(Box::new(def)))
}

impl Definer<'_, '_> {
    /// `transformTableLikeClause`: the relation must be a table, a view, a materialized view, a composite type or a partitioned table, and the user needs `SELECT` on it, or `USAGE` on a composite type. Each column that is not dropped becomes a column of the plan, with its default and its generation expression for `INCLUDING DEFAULTS` and `INCLUDING GENERATED`, its identity for `INCLUDING IDENTITY`, and its storage and compression for `INCLUDING STORAGE` and `INCLUDING COMPRESSION`. The not-null constraints of the relation are always copied, with their names. PostgreSQL adds the defaults with `ALTER TABLE` after the table exists, but the default of a column of the plan maps the column numbers in the same way.
    pub(super) fn table_like(&mut self, plan: &mut Plan, like: &TableLikeClause) -> Result<Like> {
        let rv = like.relation.as_deref().cloned().unwrap_or_default();
        let at = place(rv.location);
        let found = self.lookup_relation(&rv).map_err(|e| e.at_opt(at))?;
        let Found::User(oid) = found else {
            return Err(not_yet("LIKE of a system catalog", at));
        };
        let kind = self.kind_of(found);
        let rel = self
            .catalog
            .relation(oid)
            .ok_or_else(|| Error::internal(format!("cache lookup failed for relation {oid}")))?;
        let (name, row_type) = (rel.name.clone(), rel.row_type);
        if !matches!(kind, b'r' | b'v' | b'm' | b'c' | b'f' | b'p') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("relation \"{name}\" is invalid in LIKE clause"),
            )
            .at_opt(at)
            .with_detail(relkind_detail(kind)));
        }
        if kind == b'c' {
            self.check_type_usage(row_type)?;
        } else {
            self.check_relation_select(oid, kind, &name)?;
        }
        let options = like.options;
        let columns: Vec<(i16, rupg_catalog::Column)> = self
            .catalog
            .relation(oid)
            .ok_or_else(|| Error::internal(format!("cache lookup failed for relation {oid}")))?
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.dropped)
            .map(|(i, c)| (i16::try_from(i + 1).unwrap_or(i16::MAX), c.clone()))
            .collect();
        for (attnum, c) in columns {
            let wanted = if c.generated != 0 { options & GENERATED } else { options & DEFAULTS };
            let default = (c.has_default && wanted != 0)
                .then(|| self.catalog.default_of(oid, attnum))
                .flatten()
                .map(|d| RawDefault::Cooked(d.expr.clone(), oid));
            let generated = if c.has_default && options & GENERATED != 0 { c.generated } else { 0 };
            let mut identity = 0;
            if c.identity != 0 && options & IDENTITY != 0 {
                let info = self
                    .catalog
                    .identity_sequence(oid, attnum)
                    .and_then(|seq| self.catalog.relation(seq))
                    .and_then(|r| r.sequence.clone())
                    .ok_or_else(|| {
                        Error::internal(format!("cache lookup failed for the sequence of {name}"))
                    })?;
                let cycle = DefElem {
                    defname: Some("cycle".into()),
                    arg: Some(Node::Boolean(info.cycle)),
                    location: -1,
                    ..DefElem::default()
                };
                let seq_options = vec![
                    number_option("cache", info.cache),
                    Some(Node::DefElem(Box::new(cycle))),
                    number_option("increment", info.increment),
                    number_option("maxvalue", info.max),
                    number_option("minvalue", info.min),
                    number_option("start", info.start),
                ];
                let sequence = self.identity_sequence(
                    &plan.relname,
                    plan.namespace,
                    &c.name,
                    c.ty,
                    &seq_options,
                )?;
                plan.serials.push(Serial {
                    name: sequence.name,
                    namespace: sequence.namespace,
                    ty: c.ty,
                    column: plan.columns.len(),
                    identity: Some(sequence.options),
                    oid: 0,
                });
                identity = c.identity;
            }
            let storage = if options & STORAGE != 0 {
                storage_name(super::inherit::storage_of(c.ty, c.storage))
            } else {
                None
            };
            let compression =
                if options & COMPRESSION != 0 { compression_name(c.compression) } else { None };
            let type_name =
                TypeName { typeOid: c.ty, typemod: c.typmod, location: -1, ..TypeName::default() };
            plan.columns.push(NewColumn {
                name: c.name.clone(),
                type_name,
                collate: None,
                collation: c.collation,
                is_not_null: false,
                default,
                identity,
                generated,
                storage: storage.map(str::to_string),
                compression: compression.map(str::to_string),
                inherited: None,
            });
        }
        let rel = self
            .catalog
            .relation(oid)
            .ok_or_else(|| Error::internal(format!("cache lookup failed for relation {oid}")))?;
        let not_nulls: Vec<Constraint> = self
            .catalog
            .constraints_of(oid)
            .filter(|c| c.kind == ConKind::NotNull)
            .filter_map(|c| {
                let column = rel.column(*c.keys.first()?)?;
                Some(Constraint {
                    contype: ConstrType::CONSTR_NOTNULL,
                    conname: Some(c.name.as_str().into()),
                    keys: vec![Some(Node::String(column.name.as_str().into()))],
                    is_no_inherit: c.no_inherit,
                    is_enforced: true,
                    initially_valid: true,
                    location: -1,
                    ..Constraint::default()
                })
            })
            .collect();
        plan.not_nulls.extend(not_nulls);
        Ok(Like { relation: oid, options })
    }

    /// `expandTableLikeClause` for the new table `table`: the comments of the columns and of the not-null constraints for `INCLUDING COMMENTS`, the check constraints for `INCLUDING CONSTRAINTS`, the indexes with their constraints for `INCLUDING INDEXES`, and the statistics objects for `INCLUDING STATISTICS`, with the column numbers of the new table. A check constraint or an index that reads the whole row is an error. The new table is empty, so the check constraints are valid.
    pub(super) fn expand_table_like(&mut self, table: u32, like: &Like) -> Result<()> {
        let source = like.relation;
        let source_name = self.catalog.relation(source).map(|r| r.name.clone()).unwrap_or_default();
        let map = self.att_map(source, table)?;
        let comments = like.options & COMMENTS != 0;
        if comments {
            self.copy_like_comments(source, table)?;
        }
        if like.options & CONSTRAINTS != 0 {
            let mut checks: Vec<(String, Option<String>, bool)> = self
                .catalog
                .constraints_of(source)
                .filter(|c| c.kind == ConKind::Check)
                .map(|c| (c.name.clone(), c.expr.clone(), c.no_inherit))
                .collect();
            // `CheckConstraintFetch` sorts the check constraints by their names.
            checks.sort_by(|a, b| a.0.cmp(&b.0));
            let mut cooked = Vec::with_capacity(checks.len());
            for (name, text, no_inherit) in &checks {
                let text = text.as_deref().unwrap_or_default();
                let expr = crate::node::read(text)?;
                if column_numbers(&[&expr]).contains(&0) {
                    return Err(whole_row(format!(
                        "Constraint \"{name}\" contains a whole-row reference to table \"{source_name}\"."
                    )));
                }
                cooked.push(Constraint {
                    contype: ConstrType::CONSTR_CHECK,
                    conname: Some(name.as_str().into()),
                    cooked_expr: Some(map.text(text)?.as_str().into()),
                    is_no_inherit: *no_inherit,
                    is_enforced: true,
                    initially_valid: true,
                    skip_validation: true,
                    location: -1,
                    ..Constraint::default()
                });
            }
            let added = self.add_checks(table, &cooked)?;
            if comments {
                for ((name, ..), new) in checks.iter().zip(added) {
                    let old = self
                        .catalog
                        .constraints_of(source)
                        .find(|c| c.kind == ConKind::Check && c.name == *name)
                        .map(|c| c.oid);
                    if let Some(text) = old.and_then(|o| self.description(PG_CONSTRAINT, o, 0)) {
                        self.catalog.set_description(ObjRef::new(PG_CONSTRAINT, new), Some(text));
                    }
                }
            }
        }
        if like.options & INDEXES != 0 {
            let mut indexes: Vec<u32> = self
                .catalog
                .relations()
                .filter(|r| r.index.as_ref().is_some_and(|i| i.table == source))
                .map(|r| r.oid)
                .collect();
            indexes.sort_unstable();
            for index in indexes {
                let rel = self.catalog.relation(index);
                let index_name = rel.map(|r| r.name.clone()).unwrap_or_default();
                let info = rel.and_then(|r| r.index.as_ref());
                let mut exprs = match info.and_then(|i| i.exprs.as_deref()) {
                    Some(text) => crate::node::read_list(text)?,
                    None => Vec::new(),
                };
                if let Some(text) = info.and_then(|i| i.predicate.as_deref()) {
                    exprs.push(crate::node::read(text)?);
                }
                if column_numbers(&exprs.iter().collect::<Vec<_>>()).contains(&0) {
                    return Err(whole_row(format!(
                        "Index \"{index_name}\" contains a whole-row table reference."
                    )));
                }
                let constraint = self
                    .catalog
                    .constraints_of(source)
                    .find(|c| {
                        c.index == index && matches!(c.kind, ConKind::Primary | ConKind::Unique)
                    })
                    .map(|c| c.oid);
                // `generateClonedIndexStmt` marks the statement as transformed, so `DefineIndex` treats it as `ALTER TABLE ... ADD`.
                let (new, _) = self.clone_index(table, index, constraint, true)?;
                if self
                    .catalog
                    .relation(table)
                    .is_some_and(|r| r.kind == rupg_catalog::RelKind::Table)
                {
                    self.scans.push(table);
                }
                if comments && let Some(text) = self.description(PG_CLASS, index, 0) {
                    self.catalog.set_description(ObjRef::new(PG_CLASS, new), Some(text));
                }
            }
        }
        if like.options & STATISTICS != 0 {
            let mut stats: Vec<u32> =
                self.catalog.stat_exts().filter(|s| s.relation == source).map(|s| s.oid).collect();
            // `RelationGetStatExtList` gives the statistics objects in the order of their OIDs.
            stats.sort_unstable();
            for stat in stats {
                self.clone_statistics(stat, table, &map, comments)?;
            }
        }
        Ok(())
    }

    /// The comments of `transformTableLikeClause` for `INCLUDING COMMENTS`: each column of the new table with the name of a column of `source` gets its comment, and each not-null constraint of the new table with the name of a not-null constraint of `source` gets its comment.
    fn copy_like_comments(&mut self, source: u32, table: u32) -> Result<()> {
        let (Some(old), Some(new)) = (self.catalog.relation(source), self.catalog.relation(table))
        else {
            return Ok(());
        };
        let mut set: Vec<(ObjRef, String)> = Vec::new();
        for (i, c) in old.columns.iter().enumerate().filter(|(_, c)| !c.dropped) {
            let attnum = i32::try_from(i + 1).unwrap_or(i32::MAX);
            let Some(text) = self.description(PG_CLASS, source, attnum) else { continue };
            if let Some(number) = new.column_number(&c.name) {
                set.push((ObjRef::column(table, number), text));
            }
        }
        for con in self.catalog.constraints_of(source).filter(|c| c.kind == ConKind::NotNull) {
            let Some(text) = self.description(PG_CONSTRAINT, con.oid, 0) else { continue };
            let found = self
                .catalog
                .constraints_of(table)
                .find(|c| c.kind == ConKind::NotNull && c.name == con.name);
            if let Some(found) = found {
                set.push((ObjRef::new(PG_CONSTRAINT, found.oid), text));
            }
        }
        for (object, text) in set {
            self.catalog.set_description(object, Some(text));
        }
        Ok(())
    }

    /// `GetComment`: the comment of an object, or `None`.
    fn description(&self, class: u32, oid: u32, sub: i32) -> Option<String> {
        self.catalog.description(ObjRef { class, oid, sub }).map(str::to_string)
    }
}
