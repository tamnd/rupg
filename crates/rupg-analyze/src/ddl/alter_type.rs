//! `ALTER TABLE ... ALTER COLUMN ... TYPE`, as `transformAlterTableStmt`, `ATPrepAlterColumnType`, `ATExecAlterColumnType` and `ATPostAlterTypeCleanup` run it.
//!
//! The checks of a subcommand come first, with the transform of its `USING` expression. Then the column gets its new type in the pass `AT_PASS_ALTER_TYPE`, and its default gets the new type. After the new columns, the indexes and the constraints on the changed columns get the new types: an index gets the operator class of the new type, a check constraint is parsed again from its text, and a foreign key gets new equality operators. The session writes the rows again with the new values.

use std::collections::BTreeSet;

use rupg_catalog::{
    Column, ConKind, ObjRef, PG_ATTRDEF, PG_CLASS, PG_CONSTRAINT, PG_POLICY, PG_REWRITE,
    PG_STATISTIC_EXT, PG_TRIGGER, RelKind, Target as DropTarget, flag,
};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{
    AlterSeqStmt, AlterTableCmd, ColumnDef, ColumnRef, ConstrType, Constraint, DefElem, Node,
    RangeVar,
};

use super::alter::Target;
use super::fkey::Keys;
use super::index::no_default_opclass;
use super::table::{SYSTEM_COLUMNS, check_attribute_type};
use super::{Definer, Transform, not_yet, quote_identifier, references};
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::typcache::{default_opclass, is_binary_coercible};
use crate::typename::place;
use crate::types;

/// A subcommand of `AT_PASS_ALTER_TYPE` after its checks.
pub(super) struct TypeCmd {
    /// The name of the column.
    column: String,
    /// The column with its new type, type modifier, collation and number of array dimensions.
    new: Column,
}

/// The objects on the changed columns that `ATPostAlterTypeCleanup` makes again, by OID.
#[derive(Default)]
pub(super) struct Changed {
    indexes: BTreeSet<u32>,
    checks: BTreeSet<u32>,
    foreign: BTreeSet<u32>,
}

impl Changed {
    /// The check constraints that a change of a generation expression makes again.
    pub(super) fn add_checks(&mut self, checks: impl IntoIterator<Item = u32>) {
        self.checks.extend(checks);
    }
}

/// The error of `ATPrepAlterColumnType` for a column that the table does not have.
fn no_column(column: &str, table: &str) -> Error {
    Error::new(
        SqlState::UNDEFINED_COLUMN,
        format!("column \"{column}\" of relation \"{table}\" does not exist"),
    )
}

impl Definer<'_, '_> {
    /// The transform of `USING` and the `ALTER SEQUENCE ... AS` of an identity column that `transformAlterTableStmt` makes, then `ATPrepAlterColumnType`: the checks of the new type and of the expression that gives the new values.
    pub(super) fn prep_alter_type(
        &mut self,
        target: &Target,
        cmd: &AlterTableCmd,
        recursing: bool,
    ) -> Result<TypeCmd> {
        let Some(Node::ColumnDef(def)) = &cmd.def else {
            return Err(Error::internal("ALTER COLUMN ... TYPE without a column"));
        };
        let column = cmd.name.as_deref().unwrap_or_default().to_string();
        self.table_scope(target.oid);
        let using = match &def.raw_default {
            Some(node) => Some(self.an.with_kind(Kind::Transform, |an| an.transform(Some(node)))?),
            None => None,
        };
        let rel = self.catalog.relation(target.oid);
        let number = rel.and_then(|r| r.column_number(&column));
        let identity =
            number.and_then(|n| rel.and_then(|r| r.column(n))).is_some_and(|c| c.identity != 0);
        let row_type = rel.map_or(0, |r| r.row_type);
        let Some(number) = number else {
            // A composite type has no system columns.
            if target.kind != b'c' && SYSTEM_COLUMNS.contains(&column.as_str()) {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cannot alter system column \"{column}\""),
                )
                .at_opt(place(def.location)));
            }
            return Err(no_column(&column, &target.name));
        };
        // `USING` would give values that are not the values of the generation expression.
        let generated = rel.and_then(|r| r.column(number)).is_some_and(|c| c.generated != 0);
        if generated && using.is_some() {
            return Err(Error::new(
                SqlState::INVALID_COLUMN_DEFINITION,
                "cannot specify USING when altering type of generated column",
            )
            .with_detail(format!("Column \"{column}\" is a generated column."))
            .at_opt(place(def.location)));
        }
        if identity {
            self.alter_identity_type(target, number, def)?;
        }
        // The recursion from the parent table checked an inherited column.
        let inherited = self.catalog.relation(target.oid).and_then(|r| r.column(number));
        if inherited.is_some_and(|c| c.inhcount > 0) && !recursing {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("cannot alter inherited column \"{column}\""),
            )
            .at_opt(place(def.location)));
        }
        if self.in_partition_key(target.oid, number)? {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!(
                    "cannot alter column \"{column}\" because it is part of the partition key of relation \"{}\"",
                    target.name
                ),
            )
            .at_opt(place(def.location)));
        }
        let type_name = def.typeName.as_deref().cloned().unwrap_or_default();
        let (ty, typmod) = self.an.type_name(&type_name)?;
        self.check_type_usage(ty)?;
        let collation = match def.collClause.as_deref() {
            Some(coll) => {
                let at = place(coll.location);
                let oid = self.an.collation_oid(&coll.collname, at)?;
                if !types::collatable(ty) {
                    return Err(Error::new(
                        SqlState::DATATYPE_MISMATCH,
                        format!("collations are not supported by type {}", types::name(ty)),
                    )
                    .at_opt(at));
                }
                oid
            }
            None => types::row(ty).map_or(0, |t| t.collation),
        };
        check_attribute_type(&column, ty, false)?;
        self.check_member_of_itself(ty, row_type)?;
        let mut new = Column::new(column.clone(), ty, typmod, collation);
        new.ndims = i16::try_from(type_name.arrayBounds.len()).unwrap_or(i16::MAX);
        // A composite type and a foreign table have no rows, so the old values need no expression, but a stored value of the row type keeps its fields.
        if matches!(target.kind, b'c' | b'f') {
            if using.is_some() {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not a table", target.name),
                ));
            }
            self.check_row_type_users(target)?;
            return Ok(TypeCmd { column, new });
        }
        let given = using.is_some();
        let expr = match using {
            Some(expr) => expr,
            None => {
                let field = Some(Node::String(column.as_str().into()));
                let node: Node = ColumnRef { fields: vec![field], location: -1 }.into();
                self.an.with_kind(Kind::Transform, |an| an.transform(Some(&node)))?
            }
        };
        let coerced = self.an.coerce_to_target(expr, ty, typmod, Context::Assignment, None)?;
        if coerced.is_none() {
            let shown = types::name(ty);
            return Err(if given {
                Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!(
                        "result of USING clause for column \"{column}\" cannot be cast automatically to type {shown}"
                    ),
                )
                .with_hint("You might need to add an explicit cast.")
            } else {
                Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("column \"{column}\" cannot be cast automatically to type {shown}"),
                )
                .with_hint(format!(
                    "You might need to specify \"USING {}::{}\".",
                    quote_identifier(&column),
                    types::with_typmod(ty, typmod)
                ))
            });
        }
        if let Some(node) = &def.raw_default {
            self.transforms.push(Transform {
                table: target.oid,
                column: column.clone(),
                expr: node.clone(),
            });
        }
        Ok(TypeCmd { column, new })
    }

    /// The `ALTER SEQUENCE ... AS` that `transformAlterTableStmt` runs before the subcommands for an identity column: the sequence gets the new type.
    fn alter_identity_type(&mut self, target: &Target, number: i16, def: &ColumnDef) -> Result<()> {
        let Some(sequence) = self.catalog.identity_sequence(target.oid, number) else {
            return Ok(());
        };
        let (namespace, name) = self
            .catalog
            .relation(sequence)
            .map(|r| (r.namespace, r.name.clone()))
            .unwrap_or_default();
        let rv = RangeVar {
            schemaname: Some(self.schema_name(namespace).into()),
            relname: Some(name.into()),
            inh: true,
            location: -1,
            ..RangeVar::default()
        };
        let option = DefElem {
            defname: Some("as".into()),
            arg: def.typeName.as_deref().cloned().map(Node::from),
            location: -1,
            ..DefElem::default()
        };
        let stmt = AlterSeqStmt {
            sequence: Some(Box::new(rv)),
            options: vec![Some(option.into())],
            for_identity: true,
            missing_ok: false,
        };
        self.alter_sequence(&stmt)
    }

    /// `ATExecAlterColumnType`: the column gets its new type, and its default gets the new type. The indexes and the constraints on the column go to `changed`. `original` holds the columns of the table before the pass.
    pub(super) fn exec_alter_type(
        &mut self,
        target: &Target,
        cmd: &TypeCmd,
        original: &[Column],
        changed: &mut Changed,
    ) -> Result<()> {
        let table = target.oid;
        let rel = self.catalog.relation(table);
        let Some(number) = rel.and_then(|r| r.column_number(&cmd.column)) else {
            return Err(no_column(&cmd.column, &target.name));
        };
        let current = rel.and_then(|r| r.column(number)).cloned();
        let before = usize::try_from(number - 1).ok().and_then(|i| original.get(i));
        let (Some(current), Some(before)) = (current, before) else {
            return Err(no_column(&cmd.column, &target.name));
        };
        if current.ty != before.ty || current.typmod != before.typmod {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cannot alter type of column \"{}\" twice", cmd.column),
            ));
        }
        let (ty, typmod) = (cmd.new.ty, cmd.new.typmod);
        let default = match self.catalog.default_of(table, number) {
            Some(default) => {
                let expr = crate::node::read(&default.expr)?.strip_implicit().clone();
                let coerced =
                    self.an.coerce_to_target(expr, ty, typmod, Context::Assignment, None)?;
                let Some(coerced) = coerced else {
                    return Err(Error::new(
                        SqlState::DATATYPE_MISMATCH,
                        format!(
                            "default for column \"{}\" cannot be cast automatically to type {}",
                            cmd.column,
                            types::name(ty)
                        ),
                    ));
                };
                Some(coerced)
            }
            None => None,
        };
        let me = ObjRef::column(table, number);
        let dependents: Vec<ObjRef> = self
            .catalog
            .depends()
            .iter()
            .filter(|d| d.referenced == me)
            .map(|d| d.object)
            .collect();
        for object in dependents {
            match object.class {
                PG_REWRITE => {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "cannot alter type of a column used by a view or rule",
                    )
                    .with_detail(format!(
                        "{} depends on column \"{}\".",
                        self.describe(object),
                        cmd.column
                    )));
                }
                PG_POLICY => {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "cannot alter type of a column used in a policy definition",
                    )
                    .with_detail(format!(
                        "{} depends on column \"{}\".",
                        self.describe(object),
                        cmd.column
                    )));
                }
                // A trigger can depend on the column for `UPDATE OF` or for its condition of `WHEN`, and PostgreSQL does not change either one.
                PG_TRIGGER => {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "cannot alter type of a column used in a trigger definition",
                    )
                    .with_detail(format!(
                        "{} depends on column \"{}\".",
                        self.describe(object),
                        cmd.column
                    )));
                }
                PG_CLASS if object.sub == 0 => {
                    match self.catalog.relation(object.oid).map(|r| r.kind) {
                        Some(RelKind::Index | RelKind::PartitionedIndex) => {
                            changed.indexes.insert(object.oid);
                        }
                        // The sequence of a `serial` column or of an identity column needs no change.
                        Some(RelKind::Sequence) => {}
                        _ => {
                            return Err(Error::internal(format!(
                                "unexpected object depending on column: {}",
                                self.describe(object)
                            )));
                        }
                    }
                }
                PG_CONSTRAINT => {
                    match self.catalog.constraint(object.oid).map(|c| (c.kind, c.index)) {
                        Some((ConKind::Check, _)) => {
                            changed.checks.insert(object.oid);
                        }
                        Some((ConKind::Foreign, _)) => {
                            changed.foreign.insert(object.oid);
                        }
                        // The index of a primary key or a unique constraint depends on the constraint, not on the column.
                        Some((ConKind::Primary | ConKind::Unique, index)) if index != 0 => {
                            changed.indexes.insert(index);
                        }
                        // A not-null constraint needs no change.
                        _ => {}
                    }
                }
                // The default of the column gets the new type above.
                PG_ATTRDEF
                    if self
                        .catalog
                        .default_of(table, number)
                        .is_some_and(|d| d.oid == object.oid) => {}
                // `ATExecAlterColumnType` takes another default as the expression of a generated column.
                PG_ATTRDEF => {
                    let default = self.catalog.defaults().find(|d| d.oid == object.oid);
                    let rel = default.and_then(|d| self.catalog.relation(d.relation));
                    let column = default.and_then(|d| rel?.column(d.column));
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "cannot alter type of a column used by a generated column",
                    )
                    .with_detail(format!(
                        "Column \"{}\" is used by generated column \"{}\".",
                        cmd.column,
                        column.map_or("", |c| c.name.as_str())
                    )));
                }
                // A statistics object on the column with no expressions keeps its column numbers.
                PG_STATISTIC_EXT
                    if self.catalog.stat_ext(object.oid).is_some_and(|s| s.exprs.is_none()) => {}
                _ => {
                    return Err(not_yet(
                        &format!(
                            "ALTER COLUMN ... TYPE of a column that {} depends on",
                            self.describe(object)
                        ),
                        None,
                    ));
                }
            }
        }
        self.catalog.alter_column_type(table, number, &cmd.new)?;
        if let Some(expr) = default {
            self.catalog.remove_default(table, number);
            let refs = references(self.catalog, table, &[&expr])?;
            self.catalog.add_default(table, number, crate::node::write(&expr)?, &refs)?;
        }
        Ok(())
    }

    /// `ATPostAlterTypeCleanup`: the indexes, the check constraints and the foreign keys on the changed columns get the new types, in the order of their OIDs.
    pub(super) fn after_alter_type(
        &mut self,
        original: &[Column],
        changed: &Changed,
    ) -> Result<()> {
        for &index in &changed.indexes {
            self.rebuild_index(index, original)?;
        }
        for &check in &changed.checks {
            self.recook_check(check)?;
        }
        for &foreign in &changed.foreign {
            self.foreign_after_alter_type(foreign)?;
        }
        Ok(())
    }

    /// The index that `ATPostAlterTypeParse` makes again from its definition: a key column gets the default operator class of its new type when it had the default operator class of its old type, and it keeps a collation that the definition gave.
    fn rebuild_index(&mut self, index: u32, original: &[Column]) -> Result<()> {
        let Some(info) = self.catalog.relation(index).and_then(|r| r.index.clone()) else {
            return Err(Error::internal(format!("cache lookup failed for index {index}")));
        };
        if info.exprs.is_some() || info.predicate.is_some() {
            return Err(not_yet(
                "ALTER COLUMN ... TYPE of a column of an index with expressions or a predicate",
                None,
            ));
        }
        for (i, &key) in info.keys.iter().enumerate() {
            let at = usize::try_from(key).ok().and_then(|k| k.checked_sub(1));
            let Some(at) = at else { continue };
            let now = self.catalog.relation(info.table).and_then(|r| r.columns.get(at)).cloned();
            let (Some(now), Some(old)) = (now, original.get(at)) else { continue };
            if now.ty == old.ty && now.typmod == old.typmod && now.collation == old.collation {
                continue;
            }
            let (Some(&class), Some(&collation)) = (info.classes.get(i), info.collations.get(i))
            else {
                // An `INCLUDE` column has no operator class and no collation.
                self.catalog.set_index_column(index, i, 0, 0, &now)?;
                continue;
            };
            let class = if default_opclass(old.ty, info.method).is_some_and(|c| c.oid == class) {
                default_opclass(now.ty, info.method)
                    .map(|c| c.oid)
                    .ok_or_else(|| no_default_opclass(now.ty, info.method))?
            } else {
                let row = builtin::opclasses().iter().find(|c| c.oid == class);
                let Some(row) = row else {
                    return Err(Error::internal(format!(
                        "cache lookup failed for opclass {class}"
                    )));
                };
                if !is_binary_coercible(now.ty, row.intype) {
                    return Err(Error::new(
                        SqlState::DATATYPE_MISMATCH,
                        format!(
                            "operator class \"{}\" does not accept data type {}",
                            row.name,
                            types::name(now.ty)
                        ),
                    ));
                }
                class
            };
            // The definition gives `COLLATE` only for a collation that is not the collation of the column.
            let collation = if collation == old.collation {
                now.collation
            } else if !types::collatable(now.ty) {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("collations are not supported by type {}", types::name(now.ty)),
                ));
            } else {
                collation
            };
            self.catalog.set_index_column(index, i, class, collation, &now)?;
        }
        Ok(())
    }

    /// The check constraint that `ATPostAlterTypeParse` makes again from its text, with the same name. The new constraint has a new OID, so the session checks the rows against it. The check of a domain over a composite type whose field gets a new type is made again as `AlterDomainAddConstraint` makes it.
    fn recook_check(&mut self, oid: u32) -> Result<()> {
        let Some(con) = self.catalog.constraint(oid).cloned() else {
            return Err(Error::internal(format!("cache lookup failed for constraint {oid}")));
        };
        let text = self.an.env.deparse(con.expr.as_deref().unwrap_or_default(), con.relation)?;
        let (list, _) = rupg_sql::parse(&format!("SELECT {text}")).map_err(Error::from)?;
        let raw = list.into_iter().flatten().find_map(|node| match node {
            Node::RawStmt(raw) => match raw.stmt {
                Some(Node::SelectStmt(select)) => {
                    match select.targetList.into_iter().flatten().next() {
                        Some(Node::ResTarget(target)) => target.val,
                        _ => None,
                    }
                }
                _ => None,
            },
            _ => None,
        });
        let Some(raw) = raw else {
            return Err(Error::internal("the text of a check constraint is not an expression"));
        };
        let object = ObjRef::new(PG_CONSTRAINT, oid);
        self.catalog.remove(&[DropTarget { object, flags: flag::ORIGINAL, dependee: None }])?;
        if con.domain != 0 {
            let check = Constraint {
                contype: ConstrType::CONSTR_CHECK,
                conname: Some(con.name.as_str().into()),
                raw_expr: Some(raw),
                location: -1,
                ..Constraint::default()
            };
            let found = self.catalog.type_by_oid(con.domain).and_then(|t| t.domain.as_ref());
            let Some(base) = found.map(|d| (d.base, d.typmod)) else {
                return Err(Error::internal(format!(
                    "cache lookup failed for type {}",
                    con.domain
                )));
            };
            return self.add_domain_check(con.domain, &check, base, true);
        }
        let check = Constraint {
            contype: ConstrType::CONSTR_CHECK,
            conname: Some(con.name.as_str().into()),
            raw_expr: Some(raw),
            is_no_inherit: con.no_inherit,
            initially_valid: con.enforced,
            is_enforced: con.enforced,
            location: -1,
            ..Constraint::default()
        };
        self.table_scope(con.relation);
        let added = self
            .add_checks(con.relation, std::slice::from_ref(&check), true)
            .map_err(|e| e.with_position(None))?;
        // The new constraint of a partition keeps the count of its parent table, and a `NOT VALID` or `NOT ENFORCED` constraint stays not valid.
        for oid in added {
            self.catalog.set_constraint_inherited(oid, con.local, con.inhcount)?;
            self.catalog.set_constraint_validated(oid, con.validated)?;
        }
        Ok(())
    }

    /// The equality operators of a foreign key after a change of the type of a column on either side, as the foreign key that `ATPostAlterTypeParse` makes again has them.
    fn foreign_after_alter_type(&mut self, oid: u32) -> Result<()> {
        let con = self.catalog.constraint(oid).cloned();
        let Some((con, foreign)) = con.and_then(|c| c.foreign.clone().map(|f| (c, f))) else {
            return Err(Error::internal(format!("cache lookup failed for constraint {oid}")));
        };
        let info = self.catalog.relation(con.index).and_then(|r| r.index.as_ref());
        let classes: Option<Vec<u32>> = foreign
            .keys
            .iter()
            .map(|key| {
                let info = info?;
                let at = info.keys.iter().position(|k| k == key)?;
                info.classes.get(at).copied()
            })
            .collect();
        let Some(classes) = classes else {
            return Err(Error::internal(format!("cache lookup failed for index {}", con.index)));
        };
        let column_names = |table: u32, keys: &[i16]| -> Vec<String> {
            let rel = self.catalog.relation(table);
            keys.iter()
                .map(|&k| {
                    rel.and_then(|r| r.column(k)).map_or_else(String::new, |c| c.name.clone())
                })
                .collect()
        };
        let fk_names = column_names(con.relation, &con.keys);
        let pk_names = column_names(foreign.table, &foreign.keys);
        let keys = Keys {
            table: con.relation,
            fk: &con.keys,
            pk: foreign.table,
            pk_keys: &foreign.keys,
            classes: &classes,
        };
        let (pf_eq, pp_eq, ff_eq) =
            self.foreign_operators(&con.name, &keys, (fk_names.as_slice(), pk_names.as_slice()))?;
        self.catalog.set_foreign_operators(oid, pf_eq, pp_eq, ff_eq)
    }
}
