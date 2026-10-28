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
use super::index::{columns_collation, index_references, no_default_opclass, strip_collate};
use super::table::{SYSTEM_COLUMNS, check_attribute_type};
use super::{Definer, Transform, not_yet, quote_identifier, references};
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::expr::ExprKind;
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
                        // The index of a primary key, a unique constraint or an exclusion constraint depends on the constraint, not on the column.
                        Some((ConKind::Primary | ConKind::Unique | ConKind::Exclusion, index))
                            if index != 0 =>
                        {
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
        let old_exprs = match &info.exprs {
            Some(text) => crate::node::read_list(text)?,
            None => Vec::new(),
        };
        self.table_scope(info.table);
        // `transformIndexStmt` takes the predicate before the expressions.
        let predicate = match &info.predicate {
            Some(text) => {
                let raw = self.reparse(text, info.table)?;
                let expr =
                    self.an.with_kind(Kind::IndexPredicate, |an| an.transform(Some(&raw)))?;
                Some(self.an.coerce_to_boolean(expr, "WHERE")?)
            }
            None => None,
        };
        let mut exprs = Vec::with_capacity(old_exprs.len());
        for old in &old_exprs {
            let raw = self.reparse(&crate::node::write(old)?, info.table)?;
            let expr = self.an.with_kind(Kind::IndexExpression, |an| an.transform(Some(&raw)))?;
            exprs.push(strip_collate(&expr).clone());
        }
        let index_columns =
            self.catalog.relation(index).map(|r| r.columns.clone()).unwrap_or_default();
        let exclusion = self
            .catalog
            .constraints()
            .find(|c| c.index == index && c.kind == ConKind::Exclusion)
            .map(|c| (c.oid, c.exclusion_ops.clone()));
        let mut next = old_exprs.iter().zip(&exprs);
        // `DefineIndex` takes an expression that is a column as a key column.
        let mut keys = info.keys.clone();
        let mut kept = Vec::with_capacity(exprs.len());
        for (i, &key) in info.keys.iter().enumerate() {
            let at = usize::try_from(key).ok().and_then(|k| k.checked_sub(1));
            let Some(at) = at else {
                // `pg_get_indexdef` gives `COLLATE` for an expression only for a collation that is not the collation of the expression.
                let (Some((old, expr)), Some(column)) = (next.next(), index_columns.get(i)) else {
                    continue;
                };
                match &expr.kind {
                    ExprKind::Var(var) if var.attnum > 0 => keys[i] = var.attnum,
                    _ => kept.push(expr.clone()),
                }
                let (Some(&class), Some(&collation)) =
                    (info.classes.get(i), info.collations.get(i))
                else {
                    continue;
                };
                if expr.ty == column.ty && expr.typmod == column.typmod {
                    continue;
                }
                let now = Column::new(column.name.clone(), expr.ty, expr.typmod, 0);
                let class = self.class_after_alter_type(class, column.ty, now.ty, info.method)?;
                let collation = if collation == columns_collation(original, old) {
                    self.expr_collation(info.table, None, expr)?
                } else if !types::collatable(now.ty) {
                    return Err(Error::new(
                        SqlState::DATATYPE_MISMATCH,
                        format!("collations are not supported by type {}", types::name(now.ty)),
                    ));
                } else {
                    collation
                };
                self.catalog.set_index_column(index, i, class, collation, &now)?;
                self.exclusion_after_alter_type(exclusion.as_ref(), i, now.ty)?;
                continue;
            };
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
            let class = self.class_after_alter_type(class, old.ty, now.ty, info.method)?;
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
            self.exclusion_after_alter_type(exclusion.as_ref(), i, now.ty)?;
        }
        let exprs_text = if kept.is_empty() { None } else { Some(crate::node::write_list(&kept)?) };
        let predicate_text = predicate.as_ref().map(crate::node::write).transpose()?;
        let refs = index_references(self.catalog, info.table, &kept, predicate.as_ref())?;
        self.catalog.set_index_exprs(index, keys, exprs_text, predicate_text, &refs)
    }

    /// The exclusion operator of the key column `column` of an exclusion constraint after its type changes to `ty`: the operator of the same name for `ty` on both sides, as the constraint that `ATPostAlterTypeParse` makes again from its definition has it.
    fn exclusion_after_alter_type(
        &mut self,
        exclusion: Option<&(u32, Vec<u32>)>,
        column: usize,
        ty: u32,
    ) -> Result<()> {
        let Some((constraint, ops)) = exclusion else { return Ok(()) };
        let Some(old) = ops.get(column).and_then(|op| builtin::operator_by_oid(*op)) else {
            return Ok(());
        };
        let op = self.an.find_operator(&[old.name], ty, ty, None)?;
        self.catalog.set_exclusion_op(*constraint, column, op.oid)
    }

    /// The operator class of an index column whose type changes from `old` to `new`: the default operator class of `new` for the default operator class of `old`, else the same operator class, which must accept `new`.
    fn class_after_alter_type(&self, class: u32, old: u32, new: u32, method: u32) -> Result<u32> {
        if default_opclass(old, method).is_some_and(|c| c.oid == class) {
            return default_opclass(new, method)
                .map(|c| c.oid)
                .ok_or_else(|| no_default_opclass(new, method));
        }
        let row = builtin::opclasses().iter().find(|c| c.oid == class);
        let Some(row) = row else {
            return Err(Error::internal(format!("cache lookup failed for opclass {class}")));
        };
        if !is_binary_coercible(new, row.intype) {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "operator class \"{}\" does not accept data type {}",
                    row.name,
                    types::name(new)
                ),
            ));
        }
        Ok(class)
    }

    /// The expression that the text of a stored expression of `relation` gives when the parser reads it again, as `ATPostAlterTypeParse` reads the definition again.
    fn reparse(&self, text: &str, relation: u32) -> Result<Node> {
        let text = self.an.env.deparse(text, relation)?;
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
        raw.ok_or_else(|| Error::internal("the text of a stored expression is not an expression"))
    }

    /// The check constraint that `ATPostAlterTypeParse` makes again from its text, with the same name. The new constraint has a new OID, so the session checks the rows against it. The check of a domain over a composite type whose field gets a new type is made again as `AlterDomainAddConstraint` makes it.
    fn recook_check(&mut self, oid: u32) -> Result<()> {
        let Some(con) = self.catalog.constraint(oid).cloned() else {
            return Err(Error::internal(format!("cache lookup failed for constraint {oid}")));
        };
        let raw = self.reparse(con.expr.as_deref().unwrap_or_default(), con.relation)?;
        let object = ObjRef::new(PG_CONSTRAINT, oid);
        // `RebuildConstraintComment`: the new constraint gets the comment of the old one.
        let comment = self.catalog.description(object).map(str::to_owned);
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
            // `CommandCounterIncrement` of `ATRewriteCatalogs`: the check sees the new types of the fields.
            self.an.env.command_counter_increment(self.catalog);
            let (new, _) = self.add_domain_check(con.domain, &check, base, true)?;
            self.catalog.set_description(ObjRef::new(PG_CONSTRAINT, new), comment);
            return Ok(());
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
            self.catalog.set_description(ObjRef::new(PG_CONSTRAINT, oid), comment.clone());
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
            period: foreign.period,
        };
        let (pf_eq, pp_eq, ff_eq) =
            self.foreign_operators(&con.name, &keys, (fk_names.as_slice(), pk_names.as_slice()))?;
        self.catalog.set_foreign_operators(oid, pf_eq, pp_eq, ff_eq)
    }
}
