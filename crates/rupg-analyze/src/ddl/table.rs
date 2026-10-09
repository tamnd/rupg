//! `CREATE TABLE`, as `transformCreateStmt` and `DefineRelation` run it. Then the statements that `transformCreateStmt` adds run in the same order as in PostgreSQL: the sequences of the `serial` columns before the table, then the indexes, the foreign keys and `OWNED BY` after it.

use rupg_catalog::toast::{Shape, needs_toast};
use rupg_catalog::{Column, MAX_COLUMNS, NewRelation, SequenceInfo};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{
    CollateClause, ColumnDef, ConstrType, Constraint, CreateStmt, Equal, IndexElem, IndexStmt,
    Node, OnCommitAction, RangeVar, TypeName,
};
use rupg_types::{Value, oid};

use super::{Definer, is_system_namespace, not_yet, references, relname};
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::expr::{Expr, ExprKind, Func, FuncForm};
use crate::typename::{names, place};
use crate::types;

/// The names of the system columns.
pub(super) const SYSTEM_COLUMNS: [&str; 6] = ["tableoid", "cmax", "xmax", "cmin", "xmin", "ctid"];

/// The default of a column before `cookDefault`.
enum RawDefault {
    /// An expression of the statement.
    Expr(Node),
    /// The `nextval` call of a `serial` column, with the index of its sequence.
    Serial(usize),
}

/// A column after `transformColumnDefinition`.
struct NewColumn {
    name: String,
    type_name: TypeName,
    collate: Option<CollateClause>,
    is_not_null: bool,
    default: Option<RawDefault>,
}

/// The sequence of a `serial` column.
struct Serial {
    name: String,
    ty: u32,
    column: usize,
    oid: u32,
}

/// The state of `transformCreateStmt`, as `CreateStmtContext` keeps it.
struct Plan {
    relname: String,
    namespace: u32,
    columns: Vec<NewColumn>,
    checks: Vec<Constraint>,
    not_nulls: Vec<Constraint>,
    indexes: Vec<Constraint>,
    foreign: Vec<Constraint>,
    serials: Vec<Serial>,
    has_pkey: bool,
}

/// `makeNotNullConstraint`: an unnamed not-null constraint on the column.
fn make_not_null(column: &str) -> Constraint {
    Constraint {
        contype: ConstrType::CONSTR_NOTNULL,
        keys: vec![Some(Node::String(column.into()))],
        is_enforced: true,
        initially_valid: true,
        location: -1,
        ..Constraint::default()
    }
}

/// The column of a not-null constraint.
fn not_null_column(con: &Constraint) -> &str {
    names(&con.keys).first().copied().unwrap_or_default()
}

/// The largest value of a `serial` sequence of the type.
fn serial_max(ty: u32) -> i64 {
    match ty {
        oid::INT2 => i64::from(i16::MAX),
        oid::INT4 => i64::from(i32::MAX),
        _ => i64::MAX,
    }
}

/// `transformConstraintAttrs`: `DEFERRABLE`, `INITIALLY DEFERRED` and the other attributes change the constraint before them.
fn constraint_attrs(list: &mut [Constraint]) -> Result<()> {
    let mut last: Option<usize> = None;
    let (mut saw_deferrability, mut saw_initially, mut saw_enforced) = (false, false, false);
    for i in 0..list.len() {
        let at = place(list[i].location);
        let supports = last.is_some_and(|l| {
            matches!(
                list[l].contype,
                ConstrType::CONSTR_PRIMARY
                    | ConstrType::CONSTR_UNIQUE
                    | ConstrType::CONSTR_EXCLUSION
                    | ConstrType::CONSTR_FOREIGN
            )
        });
        let enforceable = last.is_some_and(|l| {
            matches!(list[l].contype, ConstrType::CONSTR_CHECK | ConstrType::CONSTR_FOREIGN)
        });
        let syntax = |message: &str| Error::new(SqlState::SYNTAX_ERROR, message).at_opt(at);
        match list[i].contype {
            ConstrType::CONSTR_ATTR_DEFERRABLE | ConstrType::CONSTR_ATTR_NOT_DEFERRABLE => {
                let deferrable = list[i].contype == ConstrType::CONSTR_ATTR_DEFERRABLE;
                let Some(l) = last.filter(|_| supports) else {
                    let clause = if deferrable { "DEFERRABLE" } else { "NOT DEFERRABLE" };
                    return Err(syntax(&format!("misplaced {clause} clause")));
                };
                if saw_deferrability {
                    return Err(syntax("multiple DEFERRABLE/NOT DEFERRABLE clauses not allowed"));
                }
                saw_deferrability = true;
                list[l].deferrable = deferrable;
                if !deferrable && saw_initially && list[l].initdeferred {
                    return Err(syntax(
                        "constraint declared INITIALLY DEFERRED must be DEFERRABLE",
                    ));
                }
            }
            ConstrType::CONSTR_ATTR_DEFERRED | ConstrType::CONSTR_ATTR_IMMEDIATE => {
                let deferred = list[i].contype == ConstrType::CONSTR_ATTR_DEFERRED;
                let Some(l) = last.filter(|_| supports) else {
                    let clause =
                        if deferred { "INITIALLY DEFERRED" } else { "INITIALLY IMMEDIATE" };
                    return Err(syntax(&format!("misplaced {clause} clause")));
                };
                if saw_initially {
                    return Err(syntax(
                        "multiple INITIALLY IMMEDIATE/DEFERRED clauses not allowed",
                    ));
                }
                saw_initially = true;
                list[l].initdeferred = deferred;
                if deferred {
                    if !saw_deferrability {
                        list[l].deferrable = true;
                    } else if !list[l].deferrable {
                        return Err(syntax(
                            "constraint declared INITIALLY DEFERRED must be DEFERRABLE",
                        ));
                    }
                }
            }
            ConstrType::CONSTR_ATTR_ENFORCED | ConstrType::CONSTR_ATTR_NOT_ENFORCED => {
                let enforced = list[i].contype == ConstrType::CONSTR_ATTR_ENFORCED;
                let Some(l) = last.filter(|_| enforceable) else {
                    let clause = if enforced { "ENFORCED" } else { "NOT ENFORCED" };
                    return Err(syntax(&format!("misplaced {clause} clause")));
                };
                if saw_enforced {
                    return Err(syntax("multiple ENFORCED/NOT ENFORCED clauses not allowed"));
                }
                saw_enforced = true;
                list[l].is_enforced = enforced;
                if !enforced {
                    list[l].skip_validation = true;
                    list[l].initially_valid = false;
                }
            }
            _ => {
                last = Some(i);
                saw_deferrability = false;
                saw_initially = false;
                saw_enforced = false;
            }
        }
    }
    Ok(())
}

impl Definer<'_, '_> {
    /// `transformCreateStmt`, then `DefineRelation` and the statements that it adds.
    pub(super) fn create_table(&mut self, stmt: &CreateStmt) -> Result<()> {
        let rv = stmt.relation.as_deref().cloned().unwrap_or_default();
        let at = place(rv.location);
        let namespace = self.creation_namespace(&rv)?;
        let name = relname(&rv).to_string();
        if stmt.if_not_exists && self.relation_in(namespace, &name).is_some() {
            self.notice(
                SqlState::DUPLICATE_TABLE,
                format!("relation \"{name}\" already exists, skipping"),
            );
            return Ok(());
        }
        if stmt.ofTypename.is_some() {
            return Err(not_yet("CREATE TABLE OF", at));
        }
        if stmt.partspec.is_some() || stmt.partbound.is_some() {
            return Err(not_yet("a partitioned table", at));
        }
        if !stmt.inhRelations.is_empty() {
            return Err(not_yet("INHERITS", at));
        }
        let mut plan = Plan {
            relname: name,
            namespace,
            columns: Vec::new(),
            checks: Vec::new(),
            not_nulls: Vec::new(),
            indexes: Vec::new(),
            foreign: Vec::new(),
            serials: Vec::new(),
            has_pkey: false,
        };
        for element in stmt.tableElts.iter().flatten() {
            match element {
                Node::ColumnDef(def) => self.column_definition(&mut plan, def)?,
                Node::Constraint(con) => table_constraint(&mut plan, con)?,
                _ => return Err(not_yet("CREATE TABLE LIKE", at)),
            }
        }
        for nn in &plan.not_nulls {
            let key = not_null_column(nn);
            if let Some(column) = plan.columns.iter_mut().find(|c| c.name == key) {
                column.is_not_null = true;
            }
        }
        let mut table_rv = rv.clone();
        table_rv.schemaname = Some(self.schema_name(namespace).into());
        let indexes = index_constraints(&mut plan, &table_rv)?;
        if !stmt.options.is_empty() {
            return Err(not_yet("WITH options", at));
        }
        if stmt.tablespacename.is_some() {
            return Err(not_yet("TABLESPACE", at));
        }
        if stmt.accessMethod.is_some() {
            return Err(not_yet("USING", at));
        }
        for serial in &mut plan.serials {
            let columns = vec![
                Column::new("last_value", oid::INT8, -1, 0),
                Column::new("log_cnt", oid::INT8, -1, 0),
                Column::new("is_called", oid::BOOL, -1, 0),
            ];
            let new =
                NewRelation { namespace, name: serial.name.clone(), owner: self.user, columns };
            let info = SequenceInfo {
                ty: serial.ty,
                start: 1,
                increment: 1,
                max: serial_max(serial.ty),
                min: 1,
                cache: 1,
                cycle: false,
            };
            serial.oid = self.catalog.create_sequence(new, info)?;
        }
        if stmt.oncommit != OnCommitAction::ONCOMMIT_NOOP {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "ON COMMIT can only be used on temporary tables",
            ));
        }
        let table = self.define_relation(&plan)?;
        for index in &indexes {
            self.create_index(index)?;
        }
        for fk in &plan.foreign {
            self.add_foreign_key(table, fk)?;
        }
        for serial in &plan.serials {
            let column = i16::try_from(serial.column + 1).unwrap_or(i16::MAX);
            self.catalog.set_sequence_owner(serial.oid, table, column);
        }
        Ok(())
    }

    /// `transformColumnDefinition`.
    fn column_definition(&mut self, plan: &mut Plan, def: &ColumnDef) -> Result<()> {
        let at = place(def.location);
        let name = def.colname.as_deref().unwrap_or_default().to_string();
        if def.compression.is_some() {
            return Err(not_yet("COMPRESSION", at));
        }
        if def.storage_name.is_some() {
            return Err(not_yet("STORAGE", at));
        }
        if !def.fdwoptions.is_empty() {
            return Err(not_yet("OPTIONS", at));
        }
        let mut type_name = def.typeName.as_deref().cloned().unwrap_or_default();
        let mut serial = None;
        if type_name.names.len() == 1 && !type_name.pct_type {
            serial = match names(&type_name.names).first().copied() {
                Some("smallserial" | "serial2") => Some(oid::INT2),
                Some("serial" | "serial4") => Some(oid::INT4),
                Some("bigserial" | "serial8") => Some(oid::INT8),
                _ => None,
            };
            if let Some(ty) = serial {
                type_name.names.clear();
                type_name.typeOid = ty;
                if !type_name.arrayBounds.is_empty() {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "array of serial is not implemented",
                    )
                    .at_opt(place(type_name.location)));
                }
            }
        }
        let (ty, _) = self.an.type_name(&type_name)?;
        let collate = def.collClause.as_deref().cloned();
        if let Some(coll) = &collate {
            let at = place(coll.location);
            self.an.collation_oid(&coll.collname, at)?;
            if !types::collatable(ty) {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("collations are not supported by type {}", types::name(ty)),
                )
                .at_opt(at));
            }
        }
        let mut cons: Vec<Constraint> = def
            .constraints
            .iter()
            .flatten()
            .filter_map(|n| match n {
                Node::Constraint(c) => Some((**c).clone()),
                _ => None,
            })
            .collect();
        let mut need_notnull = false;
        let mut disallow_noinherit = false;
        if let Some(ty) = serial {
            let sequence = self.catalog.choose_relation_name(
                &plan.relname,
                Some(&name),
                "seq",
                plan.namespace,
                false,
            );
            plan.serials.push(Serial { name: sequence, ty, column: plan.columns.len(), oid: 0 });
            cons.push(Constraint {
                contype: ConstrType::CONSTR_DEFAULT,
                location: -1,
                ..Constraint::default()
            });
            need_notnull = true;
            disallow_noinherit = true;
        }
        constraint_attrs(&mut cons)?;
        disallow_noinherit |= cons
            .iter()
            .any(|c| matches!(c.contype, ConstrType::CONSTR_IDENTITY | ConstrType::CONSTR_PRIMARY));
        let mut column =
            NewColumn { name: name.clone(), type_name, collate, is_not_null: false, default: None };
        let conflict = |at: Option<usize>| {
            Error::new(
                SqlState::SYNTAX_ERROR,
                format!(
                    "conflicting NULL/NOT NULL declarations for column \"{name}\" of table \"{}\"",
                    plan.relname
                ),
            )
            .at_opt(at)
        };
        let noinherit = || {
            Error::new(
                SqlState::SYNTAX_ERROR,
                format!(
                    "conflicting NO INHERIT declarations for not-null constraints on column \"{name}\""
                ),
            )
        };
        let mut saw_nullable = false;
        let mut notnull: Option<usize> = None;
        let mut new_not_nulls = Vec::new();
        let mut new_checks = Vec::new();
        let mut new_indexes = Vec::new();
        let mut new_foreign = Vec::new();
        for mut con in cons {
            let at = place(con.location);
            match con.contype {
                ConstrType::CONSTR_NULL => {
                    if (saw_nullable && column.is_not_null) || need_notnull {
                        return Err(conflict(at));
                    }
                    column.is_not_null = false;
                    saw_nullable = true;
                }
                ConstrType::CONSTR_NOTNULL => {
                    if saw_nullable && !column.is_not_null {
                        return Err(conflict(at));
                    }
                    if disallow_noinherit && con.is_no_inherit {
                        return Err(noinherit());
                    }
                    if !column.is_not_null {
                        column.is_not_null = true;
                        saw_nullable = true;
                        need_notnull = false;
                        con.keys = vec![Some(Node::String(name.as_str().into()))];
                        notnull = Some(new_not_nulls.len());
                        new_not_nulls.push(con);
                    } else if let Some(first) = notnull.map(|i| &mut new_not_nulls[i]) {
                        if let (Some(old), Some(new)) = (&first.conname, &con.conname)
                            && old != new
                        {
                            return Err(Error::internal(format!(
                                "conflicting not-null constraint names \"{old}\" and \"{new}\""
                            )));
                        }
                        if first.is_no_inherit != con.is_no_inherit {
                            return Err(noinherit());
                        }
                        if first.conname.is_none() {
                            first.conname = con.conname;
                        }
                    }
                }
                ConstrType::CONSTR_DEFAULT => {
                    if column.default.is_some() {
                        return Err(Error::new(
                            SqlState::SYNTAX_ERROR,
                            format!(
                                "multiple default values specified for column \"{name}\" of table \"{}\"",
                                plan.relname
                            ),
                        )
                        .at_opt(at));
                    }
                    column.default = Some(match con.raw_expr {
                        Some(expr) => RawDefault::Expr(expr),
                        None => RawDefault::Serial(plan.serials.len() - 1),
                    });
                }
                ConstrType::CONSTR_IDENTITY => return Err(not_yet("an identity column", at)),
                ConstrType::CONSTR_GENERATED => return Err(not_yet("a generated column", at)),
                ConstrType::CONSTR_CHECK => new_checks.push(con),
                ConstrType::CONSTR_PRIMARY | ConstrType::CONSTR_UNIQUE => {
                    if con.contype == ConstrType::CONSTR_PRIMARY {
                        if saw_nullable && !column.is_not_null {
                            return Err(conflict(at));
                        }
                        need_notnull = true;
                    }
                    if con.keys.is_empty() {
                        con.keys = vec![Some(Node::String(name.as_str().into()))];
                    }
                    new_indexes.push(con);
                }
                ConstrType::CONSTR_FOREIGN => {
                    con.fk_attrs = vec![Some(Node::String(name.as_str().into()))];
                    new_foreign.push(con);
                }
                ConstrType::CONSTR_ATTR_DEFERRABLE
                | ConstrType::CONSTR_ATTR_NOT_DEFERRABLE
                | ConstrType::CONSTR_ATTR_DEFERRED
                | ConstrType::CONSTR_ATTR_IMMEDIATE
                | ConstrType::CONSTR_ATTR_ENFORCED
                | ConstrType::CONSTR_ATTR_NOT_ENFORCED => {}
                _ => return Err(Error::internal("column exclusion constraints are not supported")),
            }
        }
        if need_notnull && !(saw_nullable && column.is_not_null) {
            column.is_not_null = true;
            new_not_nulls.push(make_not_null(&name));
        }
        plan.not_nulls.extend(new_not_nulls);
        plan.checks.extend(new_checks);
        plan.indexes.extend(new_indexes);
        plan.foreign.extend(new_foreign);
        plan.columns.push(column);
        Ok(())
    }

    /// The checks of `heap_create_with_catalog` on the name of a new relation: no relation with the name in the schema, no type with the name, and no relation in a system schema. The last check uses one OID, as PostgreSQL gives the relation its OID first.
    pub(super) fn check_relation_name(&mut self, namespace: u32, name: &str) -> Result<()> {
        if self.relation_in(namespace, name).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_TABLE,
                format!("relation \"{name}\" already exists"),
            ));
        }
        if let Some(ty) = builtin::type_by_name(namespace, name) {
            let auto_array = ty.elem != 0 && types::row(ty.elem).is_some_and(|e| e.array == ty.oid);
            if !auto_array {
                return Err(Error::new(SqlState::DUPLICATE_OBJECT, format!("type \"{name}\" already exists")).with_hint(
                    "A relation has an associated type of the same name, so you must use a name that doesn't conflict with any existing type.",
                ));
            }
            if !is_system_namespace(namespace) {
                return Err(not_yet("a table with the name of a built-in array type", None));
            }
        }
        if is_system_namespace(namespace) && !self.an.env.allow_system_table_mods() {
            self.catalog.skip_oids(1);
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to create \"{}.{}\"", self.schema_name(namespace), name),
            )
            .with_detail("System catalog modifications are currently disallowed."));
        }
        Ok(())
    }

    /// `DefineRelation` and `heap_create_with_catalog` for the table, then its defaults, its check constraints and its not-null constraints. The result is the OID of the table.
    fn define_relation(&mut self, plan: &Plan) -> Result<u32> {
        if plan.columns.len() > MAX_COLUMNS {
            return Err(Error::new(
                SqlState::TOO_MANY_COLUMNS,
                format!("tables can have at most {MAX_COLUMNS} columns"),
            ));
        }
        for (i, column) in plan.columns.iter().enumerate() {
            if plan.columns[..i].iter().any(|c| c.name == column.name) {
                return Err(Error::new(
                    SqlState::DUPLICATE_COLUMN,
                    format!("column \"{}\" specified more than once", column.name),
                ));
            }
        }
        let mut columns = Vec::with_capacity(plan.columns.len());
        for column in &plan.columns {
            let mut type_name = column.type_name.clone();
            type_name.location = -1;
            let (ty, typmod) = self.an.type_name(&type_name)?;
            if type_name.setof {
                return Err(Error::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    format!("column \"{}\" cannot be declared SETOF", column.name),
                ));
            }
            let collation = match &column.collate {
                Some(coll) => self.an.collation_oid(&coll.collname, None)?,
                None => types::row(ty).map_or(0, |t| t.collation),
            };
            let mut new = Column::new(column.name.clone(), ty, typmod, collation);
            new.ndims = i16::try_from(type_name.arrayBounds.len()).unwrap_or(i16::MAX);
            columns.push(new);
        }
        for column in &columns {
            if SYSTEM_COLUMNS.contains(&column.name.as_str()) {
                return Err(Error::new(
                    SqlState::DUPLICATE_COLUMN,
                    format!("column name \"{}\" conflicts with a system column name", column.name),
                ));
            }
        }
        for column in &columns {
            check_attribute_type(&column.name, column.ty, self.an.env.allow_system_table_mods())?;
        }
        self.check_relation_name(plan.namespace, &plan.relname)?;
        let shapes: Vec<Shape> = columns
            .iter()
            .map(|c| {
                let row = types::row(c.ty);
                Shape {
                    len: row.map_or(-1, |t| t.len),
                    align: row.map_or(b'i', |t| t.align),
                    storage: row.map_or(b'x', |t| t.storage),
                    ty: c.ty,
                    typmod: c.typmod,
                }
            })
            .collect();
        let scope: Vec<(String, u32, i32)> =
            columns.iter().map(|c| (c.name.clone(), c.ty, c.typmod)).collect();
        let new = NewRelation {
            namespace: plan.namespace,
            name: plan.relname.clone(),
            owner: self.user,
            columns,
        };
        let table = self.catalog.create_table(new)?;
        self.an.add_table(table, &plan.relname, &scope);
        for (i, column) in plan.columns.iter().enumerate() {
            let Some(default) = &column.default else { continue };
            let (ty, typmod) = (scope[i].1, scope[i].2);
            let expr = match default {
                RawDefault::Expr(node) => {
                    self.an.with_kind(Kind::ColumnDefault, |an| an.transform(Some(node)))?
                }
                RawDefault::Serial(k) => nextval(plan.serials[*k].oid)?,
            };
            let from = expr.ty;
            let Some(expr) =
                self.an.coerce_to_target(expr, ty, typmod, Context::Assignment, None)?
            else {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!(
                        "column \"{}\" is of type {} but default expression is of type {}",
                        column.name,
                        types::name(ty),
                        types::name(from)
                    ),
                )
                .with_hint("You will need to rewrite or cast the expression."));
            };
            if matches!(expr.kind, ExprKind::Const(Value::Null)) {
                continue;
            }
            let refs = references(table, &[&expr]);
            let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
            self.catalog.add_default(table, attnum, crate::node::write(&expr)?, &refs)?;
        }
        self.add_checks(table, plan)?;
        self.add_not_nulls(table, plan)?;
        if needs_toast(&shapes) {
            self.catalog.create_toast(table)?;
        }
        Ok(table)
    }

    /// The check constraints of `AddRelationNewConstraints`.
    fn add_checks(&mut self, table: u32, plan: &Plan) -> Result<()> {
        let mut names: Vec<String> = Vec::new();
        for con in &plan.checks {
            if !con.is_enforced {
                return Err(not_yet("NOT ENFORCED", place(con.location)));
            }
            let expr = self.an.with_kind(Kind::Check, |an| an.transform(con.raw_expr.as_ref()))?;
            let expr = self.an.coerce_to_boolean(expr, "CHECK")?;
            if let Some(name) = con.conname.as_deref()
                && names.iter().any(|n| n == name)
            {
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!("check constraint \"{name}\" already exists"),
                ));
            }
            let keys = super::column_numbers(&[&expr]);
            let refs = references(table, &[&expr]);
            let text = crate::node::write(&expr)?;
            let oid = self.catalog.add_check(
                table,
                con.conname.as_deref(),
                text,
                keys,
                &refs,
                con.is_no_inherit,
            )?;
            if let Some(added) = self.catalog.constraint(oid) {
                names.push(added.name.clone());
            }
        }
        Ok(())
    }

    /// `AddRelationNotNullConstraints`.
    fn add_not_nulls(&mut self, table: u32, plan: &Plan) -> Result<()> {
        let mut list = plan.not_nulls.clone();
        let mut given: Vec<String> = Vec::new();
        let mut i = 0;
        while i < list.len() {
            let key = not_null_column(&list[i]).to_string();
            let attnum = self.catalog.relation(table).and_then(|r| r.column_number(&key));
            let Some(attnum) = attnum else {
                if SYSTEM_COLUMNS.contains(&key.as_str()) {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!("cannot add not-null constraint on system column \"{key}\""),
                    ));
                }
                return Err(Error::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!("column \"{key}\" of relation \"{}\" does not exist", plan.relname),
                ));
            };
            let mut j = i + 1;
            while j < list.len() {
                if not_null_column(&list[j]) != key {
                    j += 1;
                    continue;
                }
                let other = list.remove(j);
                if other.is_no_inherit != list[i].is_no_inherit {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!(
                            "conflicting NO INHERIT declaration for not-null constraint on column \"{key}\""
                        ),
                    ));
                }
                if let Some(name) = other.conname {
                    match &list[i].conname {
                        None => list[i].conname = Some(name),
                        Some(mine) if *mine != name => {
                            return Err(Error::new(
                                SqlState::SYNTAX_ERROR,
                                format!(
                                    "conflicting not-null constraint names \"{mine}\" and \"{name}\""
                                ),
                            ));
                        }
                        Some(_) => {}
                    }
                }
            }
            let name = list[i].conname.as_deref().map(str::to_string);
            if let Some(name) = &name {
                if given.contains(name) {
                    return Err(Error::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!(
                            "constraint \"{name}\" for relation \"{}\" already exists",
                            plan.relname
                        ),
                    ));
                }
                given.push(name.clone());
                if self.catalog.constraints_of(table).any(|c| c.name == *name) {
                    return Err(Error::new(
                        SqlState::UNIQUE_VIOLATION,
                        "duplicate key value violates unique constraint \"pg_constraint_conrelid_contypid_conname_index\"",
                    )
                    .with_detail(format!(
                        "Key (conrelid, contypid, conname)=({table}, 0, {name}) already exists."
                    )));
                }
            }
            self.catalog.add_not_null(table, name.as_deref(), attnum, list[i].is_no_inherit)?;
            i += 1;
        }
        Ok(())
    }
}

/// `transformTableConstraint`.
fn table_constraint(plan: &mut Plan, con: &Constraint) -> Result<()> {
    let con = con.clone();
    match con.contype {
        ConstrType::CONSTR_PRIMARY | ConstrType::CONSTR_UNIQUE => plan.indexes.push(con),
        ConstrType::CONSTR_EXCLUSION => {
            return Err(not_yet("an exclusion constraint", place(con.location)));
        }
        ConstrType::CONSTR_CHECK => plan.checks.push(con),
        ConstrType::CONSTR_NOTNULL => plan.not_nulls.push(con),
        ConstrType::CONSTR_FOREIGN => plan.foreign.push(con),
        ConstrType(n) => {
            return Err(Error::internal(format!("invalid context for constraint type {n}")));
        }
    }
    Ok(())
}

/// `transformIndexConstraints`: the index statements of the primary key and the unique constraints. The primary key comes first, and a constraint that is the same as an earlier one adds no index.
fn index_constraints(plan: &mut Plan, rv: &RangeVar) -> Result<Vec<IndexStmt>> {
    let mut all = Vec::new();
    let mut pkey = None;
    for con in plan.indexes.clone() {
        let index = index_constraint(plan, &con, rv)?;
        if index.primary {
            pkey = Some(all.len());
        }
        all.push(index);
    }
    let mut result: Vec<IndexStmt> = Vec::new();
    if let Some(p) = pkey {
        result.push(all[p].clone());
    }
    for (i, index) in all.into_iter().enumerate() {
        if Some(i) == pkey {
            continue;
        }
        let prior = result.iter_mut().find(|prior| {
            index.indexParams.equal(&prior.indexParams)
                && index.indexIncludingParams.equal(&prior.indexIncludingParams)
                && index.whereClause.equal(&prior.whereClause)
                && index.accessMethod == prior.accessMethod
                && index.nulls_not_distinct == prior.nulls_not_distinct
                && index.deferrable == prior.deferrable
                && index.initdeferred == prior.initdeferred
        });
        match prior {
            Some(prior) => {
                prior.unique |= index.unique;
                if prior.idxname.is_none() {
                    prior.idxname = index.idxname;
                }
            }
            None => result.push(index),
        }
    }
    Ok(result)
}

/// `transformIndexConstraint` for a primary key or a unique constraint.
fn index_constraint(plan: &mut Plan, con: &Constraint, rv: &RangeVar) -> Result<IndexStmt> {
    let at = place(con.location);
    let primary = con.contype == ConstrType::CONSTR_PRIMARY;
    if primary {
        if plan.has_pkey {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("multiple primary keys for table \"{}\" are not allowed", plan.relname),
            )
            .at_opt(at));
        }
        plan.has_pkey = true;
    }
    if con.indexname.is_some() {
        return Err(Error::new(
            SqlState::SYNTAX_ERROR,
            "cannot use an existing index in CREATE TABLE",
        )
        .at_opt(at));
    }
    if con.without_overlaps {
        return Err(not_yet("WITHOUT OVERLAPS", at));
    }
    let missing = |key: &str| {
        Error::new(
            SqlState::UNDEFINED_COLUMN,
            format!("column \"{key}\" named in key does not exist"),
        )
        .at_opt(at)
    };
    let mut params: Vec<IndexElem> = Vec::new();
    for key in names(&con.keys) {
        let found = match plan.columns.iter_mut().find(|c| c.name == key) {
            Some(column) => {
                if primary {
                    if column.is_not_null {
                        let nn = plan.not_nulls.iter().find(|nn| not_null_column(nn) == key);
                        if nn.is_some_and(|nn| nn.is_no_inherit) {
                            return Err(Error::new(
                                SqlState::SYNTAX_ERROR,
                                format!(
                                    "conflicting NO INHERIT declaration for not-null constraint on column \"{key}\""
                                ),
                            ));
                        }
                    } else {
                        column.is_not_null = true;
                        plan.not_nulls.push(make_not_null(key));
                    }
                }
                true
            }
            None => SYSTEM_COLUMNS.contains(&key),
        };
        if !found {
            return Err(missing(key));
        }
        if params.iter().any(|p| p.name.as_deref() == Some(key)) {
            let what = if primary { "primary key" } else { "unique" };
            return Err(Error::new(
                SqlState::DUPLICATE_COLUMN,
                format!("column \"{key}\" appears twice in {what} constraint"),
            )
            .at_opt(at));
        }
        params.push(IndexElem { name: Some(key.into()), location: -1, ..IndexElem::default() });
    }
    let mut including = Vec::new();
    for key in names(&con.including) {
        if !plan.columns.iter().any(|c| c.name == key) && !SYSTEM_COLUMNS.contains(&key) {
            return Err(missing(key));
        }
        including.push(IndexElem { name: Some(key.into()), location: -1, ..IndexElem::default() });
    }
    let elems = |list: Vec<IndexElem>| -> Vec<Option<Node>> {
        list.into_iter().map(|e| Some(Node::IndexElem(Box::new(e)))).collect()
    };
    Ok(IndexStmt {
        idxname: con.conname.clone(),
        relation: Some(Box::new(rv.clone())),
        accessMethod: Some(con.access_method.clone().unwrap_or_else(|| "btree".into())),
        tableSpace: con.indexspace.clone(),
        indexParams: elems(params),
        indexIncludingParams: elems(including),
        options: con.options.clone(),
        whereClause: con.where_clause.clone(),
        unique: true,
        nulls_not_distinct: con.nulls_not_distinct,
        primary,
        isconstraint: true,
        deferrable: con.deferrable,
        initdeferred: con.initdeferred,
        ..IndexStmt::default()
    })
}

/// `CheckAttributeType`: a column cannot have a pseudo-type, also as the base type of a domain or the element type of an array. `anyarray` allows the type `anyarray`, as `CHKATYPE_ANYARRAY` does when `allow_system_table_mods` is on.
pub(crate) fn check_attribute_type(name: &str, ty: u32, anyarray: bool) -> Result<()> {
    let Some(row) = types::row(ty) else { return Ok(()) };
    match row.kind {
        b'p' if anyarray && ty == oid::ANYARRAY => Ok(()),
        b'p' => Err(Error::new(
            SqlState::INVALID_TABLE_DEFINITION,
            format!("column \"{name}\" has pseudo-type {}", types::name(ty)),
        )),
        b'd' => check_attribute_type(name, row.base, false),
        b'c' => Err(not_yet("a column of a composite type", None)),
        _ if types::element(ty) != 0 => check_attribute_type(name, types::element(ty), false),
        _ => Ok(()),
    }
}

/// The default of a `serial` column before the cast to the type of the column: `nextval('sequence'::regclass)`.
fn nextval(sequence: u32) -> Result<Expr> {
    let proc = builtin::procs_named("nextval")
        .next()
        .ok_or_else(|| Error::internal("function nextval does not exist"))?;
    let arg = Expr::constant(Value::Oid(sequence), oid::REGCLASS, -1, None);
    let func = Func { oid: proc.oid, args: vec![arg], form: FuncForm::Call, variadic: false };
    Ok(Expr::new(ExprKind::Func(func), proc.rettype))
}
