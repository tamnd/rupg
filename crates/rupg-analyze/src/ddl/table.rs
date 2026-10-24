//! `CREATE TABLE`, as `transformCreateStmt` and `DefineRelation` run it. Then the statements that `transformCreateStmt` adds run in the same order as in PostgreSQL: the sequences of the `serial` columns before the table, then the indexes, the foreign keys and `OWNED BY` after it.

use rupg_catalog::toast::{Shape, needs_toast};
use rupg_catalog::{
    Column, ConKind, DepKind, MAX_COLUMNS, NewRelation, RelKind, SequenceData, SequenceInfo,
};
use rupg_common::{Error, Object, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{
    CollateClause, ColumnDef, CompositeTypeStmt, ConstrType, Constraint, CreateStmt, DefElem,
    Equal, IndexElem, IndexStmt, Node, OnCommitAction, PartitionBoundSpec, PartitionSpec, RangeVar,
    TypeName,
};
use rupg_types::{Value, oid};

use super::alter::Target;
use super::alter_column::{attribute_compression, attribute_storage};
use super::comment::range_var;
use super::partition_attach::without_locations;
use super::sequence::conflicting;
use super::{
    Definer, Fill, is_mutable, is_system_namespace, not_yet, references, relkind_detail, relname,
};
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::expr::{Expr, ExprKind};
use crate::insert::nextval;
use crate::typename::{names, place};
use crate::types;

mod inherit;
mod typed;

pub(super) use inherit::collation_name;

/// The names of the system columns.
pub(super) const SYSTEM_COLUMNS: [&str; 6] = ["tableoid", "cmax", "xmax", "cmin", "xmin", "ctid"];

/// The default of a column before `cookDefault`.
enum RawDefault {
    /// An expression of the statement.
    Expr(Node),
    /// The `nextval` call of a `serial` column, with the index of its sequence.
    Serial(usize),
    /// The stored default of a parent table, with the OID of the parent. Its columns have the numbers of the parent.
    Cooked(String, u32),
}

/// A column after `transformColumnDefinition`.
struct NewColumn {
    name: String,
    type_name: TypeName,
    collate: Option<CollateClause>,
    /// `collOid`: the collation of a column of `CREATE TABLE AS`, or 0.
    collation: u32,
    is_not_null: bool,
    default: Option<RawDefault>,
    /// `attidentity`, or 0.
    identity: u8,
    /// `attgenerated`: `s` for a stored generated column, or 0. The default is then the generation expression.
    generated: u8,
    /// The name of the storage of `STORAGE`.
    storage: Option<String>,
    /// The name of the method of `COMPRESSION`.
    compression: Option<String>,
    /// The column of the parent table that gives the column of a partition.
    inherited: Option<Column>,
}

/// The name, the schema and the options of the sequence of an identity column, as `generateSerialSequence` finds them.
pub(super) struct IdentitySequence {
    pub(super) namespace: u32,
    pub(super) name: String,
    /// The options with `AS` and the type of the column first.
    pub(super) options: Vec<Option<Node>>,
}

/// The sequence of a `serial` column or of an identity column.
struct Serial {
    name: String,
    namespace: u32,
    ty: u32,
    column: usize,
    /// The options of the sequence of an identity column, with `AS` and the type of the column first, or `None` for a `serial` column.
    identity: Option<Vec<Option<Node>>>,
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
    /// The partition key of `PARTITION BY`.
    partspec: Option<PartitionSpec>,
    /// The bound of `PARTITION OF`.
    partbound: Option<PartitionBoundSpec>,
    /// The parent table of `PARTITION OF`.
    parent: Option<u32>,
    /// The parent tables of `INHERITS`, in order.
    inherits: Vec<u32>,
    /// `ofType`: true for a table of `OF type`.
    typed: bool,
}

/// `makeNotNullConstraint`: an unnamed not-null constraint on the column.
pub(super) fn make_not_null(column: &str) -> Constraint {
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
pub(super) fn not_null_column(con: &Constraint) -> &str {
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

/// The options of the sequence of a `serial` column of the type.
fn serial_info(ty: u32) -> SequenceInfo {
    SequenceInfo {
        ty,
        start: 1,
        increment: 1,
        max: serial_max(ty),
        min: 1,
        cache: 1,
        cycle: false,
        file: 0,
        data: SequenceData::new(1),
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
    /// The check of `heap_create_with_catalog` for `ON COMMIT`, which only a temporary table can have.
    pub(super) fn check_on_commit(&self, action: OnCommitAction, namespace: u32) -> Result<()> {
        if action == OnCommitAction::ONCOMMIT_NOOP {
            return Ok(());
        }
        if !self.catalog.is_temp_schema(namespace) {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "ON COMMIT can only be used on temporary tables",
            ));
        }
        match action {
            OnCommitAction::ONCOMMIT_DELETE_ROWS => Err(not_yet("ON COMMIT DELETE ROWS", None)),
            OnCommitAction::ONCOMMIT_DROP => Err(not_yet("ON COMMIT DROP", None)),
            _ => Ok(()),
        }
    }

    /// `transformCreateStmt`, then `DefineRelation` and the statements that it adds.
    pub(super) fn create_table(&mut self, stmt: &CreateStmt) -> Result<()> {
        let rv = stmt.relation.as_deref().cloned().unwrap_or_default();
        let at = place(rv.location);
        if stmt.partspec.is_some() && rv.relpersistence == b'u' {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "partitioned tables cannot be unlogged",
            ));
        }
        let namespace = self.creation_namespace(&rv)?;
        let unlogged = rv.relpersistence == b'u';
        let name = relname(&rv).to_string();
        if stmt.if_not_exists && self.relation_in(namespace, &name).is_some() {
            self.notice(
                SqlState::DUPLICATE_TABLE,
                format!("relation \"{name}\" already exists, skipping"),
            );
            return Ok(());
        }
        let of_type = stmt.ofTypename.as_deref().map(|name| self.of_type(name)).transpose()?;
        if stmt.partspec.is_some() && unlogged {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "partitioned tables cannot be unlogged",
            ));
        }
        if stmt.partspec.is_some() && !stmt.inhRelations.is_empty() && stmt.partbound.is_none() {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                "cannot create partitioned table as inheritance child",
            ));
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
            partspec: stmt.partspec.as_deref().cloned(),
            partbound: stmt.partbound.as_deref().cloned(),
            parent: None,
            inherits: Vec::new(),
            typed: stmt.ofTypename.is_some(),
        };
        // The table constraints of a partition come after the columns of the parent table, as `transformIndexConstraint` finds the key columns of a partition in its parent table.
        let mut partition_constraints = Vec::new();
        for element in stmt.tableElts.iter().flatten() {
            match element {
                Node::ColumnDef(def) => self.column_definition(&mut plan, def)?,
                Node::Constraint(con) if plan.partbound.is_some() => {
                    partition_constraints.push(con)
                }
                Node::Constraint(con) => table_constraint(&mut plan, con)?,
                _ => return Err(not_yet("CREATE TABLE LIKE", at)),
            }
        }
        if let Some(ty) = of_type {
            self.merge_type_columns(&mut plan, ty)?;
        }
        for nn in &plan.not_nulls {
            let key = not_null_column(nn);
            if let Some(column) = plan.columns.iter_mut().find(|c| c.name == key) {
                column.is_not_null = true;
            }
        }
        if plan.partbound.is_some() {
            let parent = self.partition_parent(&stmt.inhRelations, namespace)?;
            self.merge_partition_columns(&mut plan, parent)?;
            plan.parent = Some(parent);
            for con in partition_constraints {
                table_constraint(&mut plan, con)?;
            }
        } else if !stmt.inhRelations.is_empty() {
            let parents = self.inherit_parents(&stmt.inhRelations)?;
            self.merge_inherited_columns(&mut plan, &parents, namespace)?;
            plan.inherits = parents;
        }
        let mut table_rv = rv.clone();
        table_rv.schemaname = Some(self.schema_name(namespace).into());
        let indexes = index_constraints(&mut plan, &table_rv)?;
        let options = self.create_table_options(&stmt.options)?;
        if stmt.tablespacename.is_some() {
            return Err(not_yet("TABLESPACE", at));
        }
        if stmt.accessMethod.is_some() {
            return Err(not_yet("USING", at));
        }
        for serial in &mut plan.serials {
            let info = match &serial.identity {
                Some(options) => self.identity_sequence_info(options)?,
                None => serial_info(serial.ty),
            };
            serial.oid = self.new_sequence(serial.namespace, &serial.name, info, unlogged)?;
        }
        self.check_on_commit(stmt.oncommit, namespace)?;
        let table = self.define_relation(&plan)?;
        if let Some(ty) = of_type {
            self.catalog.set_of_type(table, ty)?;
        }
        if unlogged {
            self.catalog.set_unlogged(table, true)?;
        }
        if !options.is_empty() {
            self.catalog.set_options(table, options)?;
        }
        if let Some(parent) = plan.parent {
            self.clone_partition_indexes(table, parent)?;
            self.clone_triggers(parent, table)?;
            self.clone_foreign_keys(parent, table)?;
        }
        self.create_toast_options(table, &stmt.options)?;
        for index in &indexes {
            self.create_index(index, false)?;
        }
        for fk in &plan.foreign {
            self.add_foreign_key(table, fk)?;
        }
        for serial in &plan.serials {
            let column = i16::try_from(serial.column + 1).unwrap_or(i16::MAX);
            if serial.identity.is_some() {
                let owner = (table, plan.namespace, plan.relname.as_str());
                self.own_identity_sequence(serial.oid, serial.namespace, owner, column)?;
            } else {
                self.catalog.set_sequence_owner(serial.oid, table, column);
            }
        }
        Ok(())
    }

    /// `DefineSequence` for the sequence of a `serial` column or of an identity column: a relation with the columns of a sequence and the row of `pg_sequence`. The sequence is unlogged when its table is unlogged, as `generateSerialExtraStmts` gives it the persistence of the table.
    pub(super) fn new_sequence(
        &mut self,
        namespace: u32,
        name: &str,
        info: SequenceInfo,
        unlogged: bool,
    ) -> Result<u32> {
        let columns = vec![
            Column::new("last_value", oid::INT8, -1, 0),
            Column::new("log_cnt", oid::INT8, -1, 0),
            Column::new("is_called", oid::BOOL, -1, 0),
        ];
        let new = NewRelation { namespace, name: name.to_string(), owner: self.user, columns };
        let oid = self.catalog.create_sequence(new, info)?;
        if unlogged {
            self.catalog.set_unlogged(oid, true)?;
        }
        Ok(oid)
    }

    /// True when the relation is unlogged.
    pub(super) fn is_unlogged(&self, oid: u32) -> bool {
        self.catalog.relation(oid).is_some_and(|r| r.unlogged)
    }

    /// `relpersistence` of the relation: `t` in a temporary schema, `u` for an unlogged relation, else `p`.
    pub(super) fn persistence(&self, oid: u32) -> u8 {
        match self.catalog.relation(oid) {
            Some(rel) if self.catalog.is_temp_schema(rel.namespace) => b't',
            Some(rel) if rel.unlogged => b'u',
            _ => b'p',
        }
    }

    /// `rd_islocaltemp`: true for a relation in the temporary schema of the session.
    pub(super) fn is_own_temp(&self, oid: u32) -> bool {
        self.catalog.relation(oid).is_some_and(|r| r.namespace != 0 && r.namespace == self.an.temp)
    }

    /// The `OWNED BY` of the sequence of an identity column, with an internal dependency on the column. `table` is the OID, the schema and the name of the table. `generateSerialSequence` names the table for `OWNED BY` with the schema of the sequence, so a sequence in another schema does not find the table.
    pub(super) fn own_identity_sequence(
        &mut self,
        sequence: u32,
        namespace: u32,
        table: (u32, u32, &str),
        column: i16,
    ) -> Result<()> {
        let (oid, table_namespace, relname) = table;
        if namespace != table_namespace {
            return Err(Error::new(
                SqlState::UNDEFINED_TABLE,
                format!("relation \"{}.{relname}\" does not exist", self.schema_name(namespace)),
            ));
        }
        self.catalog.set_owned_by(sequence, Some((oid, column)), DepKind::Internal);
        Ok(())
    }

    /// `generateSerialSequence` for an identity column: the name and the schema of the sequence come from `SEQUENCE NAME`, or the name comes from the names of the table and the column. The other options get `AS` with the type of the column first, so that the error for an `AS` of the statement points to it.
    pub(super) fn identity_sequence(
        &mut self,
        table: &str,
        namespace: u32,
        column: &str,
        ty: u32,
        options: &[Option<Node>],
    ) -> Result<IdentitySequence> {
        let type_name = TypeName { typeOid: ty, typemod: -1, location: -1, ..TypeName::default() };
        let as_type = DefElem {
            defname: Some("as".into()),
            arg: Some(Node::TypeName(Box::new(type_name))),
            location: -1,
            ..DefElem::default()
        };
        let mut rest = vec![Some(Node::DefElem(Box::new(as_type)))];
        let mut name_option: Option<&DefElem> = None;
        for option in options {
            match option {
                Some(Node::DefElem(def)) if def.defname.as_deref() == Some("sequence_name") => {
                    if name_option.is_some() {
                        return Err(conflicting(def));
                    }
                    name_option = Some(def);
                }
                _ => rest.push(option.clone()),
            }
        }
        let (namespace, name) = match name_option {
            Some(def) => {
                let list = match &def.arg {
                    Some(Node::List(list)) => names(list),
                    _ => Vec::new(),
                };
                let rv = range_var(&list)?;
                let namespace =
                    if rv.schemaname.is_some() { self.creation_namespace(&rv)? } else { namespace };
                (namespace, relname(&rv).to_string())
            }
            None => {
                let name =
                    self.catalog.choose_relation_name(table, Some(column), "seq", namespace, false);
                (namespace, name)
            }
        };
        Ok(IdentitySequence { namespace, name, options: rest })
    }

    /// `transformColumnDefinition`.
    /// `create_ctas_internal`: `DefineRelation` for the columns of the query of `CREATE TABLE AS`. Each column has its name, its type, its typmod and its collation, as `makeColumnDef` gives them. `defs` are the options of `WITH`.
    pub(super) fn create_query_table(
        &mut self,
        namespace: u32,
        name: &str,
        columns: Vec<(String, u32, i32, u32)>,
        defs: &[Option<Node>],
    ) -> Result<u32> {
        let options = self.create_table_options(defs)?;
        let columns = columns
            .into_iter()
            .map(|(name, ty, typmod, collation)| NewColumn {
                name,
                type_name: TypeName {
                    typeOid: ty,
                    typemod: typmod,
                    location: -1,
                    ..TypeName::default()
                },
                collate: None,
                collation,
                is_not_null: false,
                default: None,
                identity: 0,
                generated: 0,
                storage: None,
                compression: None,
                inherited: None,
            })
            .collect();
        let plan = Plan {
            relname: name.to_string(),
            namespace,
            columns,
            checks: Vec::new(),
            not_nulls: Vec::new(),
            indexes: Vec::new(),
            foreign: Vec::new(),
            serials: Vec::new(),
            has_pkey: false,
            partspec: None,
            partbound: None,
            parent: None,
            inherits: Vec::new(),
            typed: false,
        };
        let table = self.define_relation(&plan)?;
        if !options.is_empty() {
            self.catalog.set_options(table, options)?;
        }
        self.create_toast_options(table, defs)?;
        Ok(table)
    }

    fn column_definition(&mut self, plan: &mut Plan, def: &ColumnDef) -> Result<()> {
        let at = place(def.location);
        let name = def.colname.as_deref().unwrap_or_default().to_string();
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
        // The column of a partition has no type: the parent table gives it.
        let ty = if def.typeName.is_some() { self.an.type_name(&type_name)?.0 } else { 0 };
        let collate = def.collClause.as_deref().cloned();
        if let Some(coll) = &collate
            && ty != 0
        {
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
            plan.serials.push(Serial {
                name: sequence,
                namespace: plan.namespace,
                ty,
                column: plan.columns.len(),
                identity: None,
                oid: 0,
            });
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
        let mut column = NewColumn {
            name: name.clone(),
            type_name,
            collate,
            collation: 0,
            is_not_null: false,
            default: None,
            identity: 0,
            generated: 0,
            storage: def.storage_name.as_deref().map(str::to_string),
            compression: def.compression.as_deref().map(str::to_string),
            inherited: None,
        };
        let relname = plan.relname.clone();
        let conflict = |at: Option<usize>| {
            Error::new(
                SqlState::SYNTAX_ERROR,
                format!(
                    "conflicting NULL/NOT NULL declarations for column \"{name}\" of table \"{relname}\""
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
        let mut saw_default = false;
        let mut saw_generated = false;
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
                    if plan.partspec.is_some() && con.is_no_inherit {
                        return Err(no_inherit_not_null());
                    }
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
                    if saw_default {
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
                    saw_default = true;
                }
                ConstrType::CONSTR_IDENTITY => {
                    if plan.partbound.is_some() {
                        return Err(Error::new(
                            SqlState::FEATURE_NOT_SUPPORTED,
                            "identity columns are not supported on partitions",
                        ));
                    }
                    if column.identity != 0 {
                        return Err(Error::new(
                            SqlState::SYNTAX_ERROR,
                            format!(
                                "multiple identity specifications for column \"{name}\" of table \"{}\"",
                                plan.relname
                            ),
                        )
                        .at_opt(at));
                    }
                    let sequence = self.identity_sequence(
                        &plan.relname,
                        plan.namespace,
                        &name,
                        ty,
                        &con.options,
                    )?;
                    plan.serials.push(Serial {
                        name: sequence.name,
                        namespace: sequence.namespace,
                        ty,
                        column: plan.columns.len(),
                        identity: Some(sequence.options),
                        oid: 0,
                    });
                    column.identity = con.generated_when;
                    // An identity column is always not null, but the column can have a constraint already.
                    if saw_nullable && !column.is_not_null {
                        return Err(conflict(at));
                    }
                    if !saw_nullable {
                        new_not_nulls.push(make_not_null(&name));
                    }
                    column.is_not_null = true;
                    saw_nullable = true;
                }
                ConstrType::CONSTR_GENERATED => {
                    if plan.typed {
                        return Err(Error::new(
                            SqlState::FEATURE_NOT_SUPPORTED,
                            "generated columns are not supported on typed tables",
                        ));
                    }
                    if saw_generated {
                        return Err(Error::new(
                            SqlState::SYNTAX_ERROR,
                            format!(
                                "multiple generation clauses specified for column \"{name}\" of table \"{}\"",
                                plan.relname
                            ),
                        )
                        .at_opt(at));
                    }
                    if con.generated_kind != b's' {
                        return Err(not_yet("a virtual generated column", at));
                    }
                    column.generated = con.generated_kind;
                    column.default = con.raw_expr.map(RawDefault::Expr);
                    saw_generated = true;
                }
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
            let both = match (saw_default, column.identity != 0, saw_generated) {
                (true, true, _) => "default and identity",
                (true, _, true) => "default and generation expression",
                (_, true, true) => "identity and generation expression",
                _ => continue,
            };
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!(
                    "both {both} specified for column \"{name}\" of table \"{}\"",
                    plan.relname
                ),
            )
            .at_opt(at));
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

    /// `DefineCompositeType`, then `DefineRelation` for the relation of the type. The errors have no place, as `DefineCompositeType` gives no parse state.
    pub(super) fn create_composite_type(&mut self, stmt: &CompositeTypeStmt) -> Result<()> {
        let rv = stmt.typevar.as_deref().cloned().unwrap_or_default();
        let namespace = self.creation_namespace(&rv).map_err(|e| e.with_position(None))?;
        let name = relname(&rv).to_string();
        self.check_type_name(namespace, &name)?;
        let defs: Vec<&ColumnDef> = stmt
            .coldeflist
            .iter()
            .flatten()
            .filter_map(|n| match n {
                Node::ColumnDef(def) => Some(&**def),
                _ => None,
            })
            .collect();
        if defs.len() > MAX_COLUMNS {
            return Err(Error::new(
                SqlState::TOO_MANY_COLUMNS,
                format!("tables can have at most {MAX_COLUMNS} columns"),
            ));
        }
        let column_name = |def: &ColumnDef| def.colname.clone().unwrap_or_default();
        for (i, def) in defs.iter().enumerate() {
            let name = column_name(def);
            if defs[..i].iter().any(|d| column_name(d) == name) {
                return Err(Error::new(
                    SqlState::DUPLICATE_COLUMN,
                    format!("column \"{name}\" specified more than once"),
                ));
            }
        }
        let mut columns = Vec::with_capacity(defs.len());
        for def in defs {
            let mut type_name = def.typeName.as_deref().cloned().unwrap_or_default();
            type_name.location = -1;
            let (ty, typmod) = self.an.type_name(&type_name)?;
            let collation = match def.collClause.as_deref() {
                Some(coll) => {
                    let collation = self.an.collation_oid(&coll.collname, None)?;
                    if !types::collatable(ty) {
                        return Err(Error::new(
                            SqlState::DATATYPE_MISMATCH,
                            format!("collations are not supported by type {}", types::name(ty)),
                        ));
                    }
                    collation
                }
                None => types::row(ty).map_or(0, |t| t.collation),
            };
            let mut column = Column::new(column_name(def), ty, typmod, collation);
            column.ndims = i16::try_from(type_name.arrayBounds.len()).unwrap_or(i16::MAX);
            columns.push(column);
        }
        for column in &columns {
            check_attribute_type(&column.name, column.ty, self.an.env.allow_system_table_mods())?;
        }
        self.check_relation_name(namespace, &name)?;
        let new = NewRelation { namespace, name, owner: self.user, columns };
        self.catalog.create_composite_type(new)?;
        Ok(())
    }

    /// `42710` when the schema has a type with the name. `moveArrayTypeName` moves an array type that a type made out of the way, so only another type is a conflict.
    pub(super) fn check_type_name(&self, namespace: u32, name: &str) -> Result<()> {
        let user = self.catalog.type_by_name(namespace, name).map(|t| (t.oid, t.element));
        let old = user.or_else(|| builtin::type_by_name(namespace, name).map(|t| (t.oid, t.elem)));
        let is_auto_array = |(ty, element): (u32, u32)| {
            element != 0
                && (self.catalog.type_by_oid(element).is_some_and(|e| e.array == ty)
                    || types::row(element).is_some_and(|e| e.array == ty))
        };
        if old.is_some_and(|old| !is_auto_array(old)) {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("type \"{name}\" already exists"),
            ));
        }
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

    /// `BuildDescForRelation` for one column: its type, its type modifier, its collation and the number of its array dimensions.
    fn catalog_column(&mut self, column: &NewColumn) -> Result<Column> {
        if let Some(parent) = &column.inherited {
            let mut new =
                Column::new(column.name.clone(), parent.ty, parent.typmod, parent.collation);
            new.storage = parent.storage;
            new.compression = parent.compression;
            // A partition has the identity of the column of its parent, as `MergeAttributes` gives it.
            new.identity = parent.identity;
            new.generated = parent.generated;
            new.local = parent.local;
            new.inhcount = parent.inhcount;
            return Ok(new);
        }
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
            None if column.collation != 0 => column.collation,
            None => types::row(ty).map_or(0, |t| t.collation),
        };
        let mut new = Column::new(column.name.clone(), ty, typmod, collation);
        new.ndims = i16::try_from(type_name.arrayBounds.len()).unwrap_or(i16::MAX);
        new.identity = column.identity;
        new.generated = column.generated;
        if let Some(name) = &column.storage {
            new.storage = attribute_storage(ty, name)?;
        }
        if let Some(name) = &column.compression {
            new.compression = attribute_compression(ty, name)?;
        }
        Ok(new)
    }

    /// The part of `MergeAttributes` for a partition: the columns of the parent table, in their order, with the defaults of the parent. A dropped column of the parent is not a column of the partition, so the later columns can have other numbers in the partition. A column of the statement only gives the constraints and a new default of a column of the parent.
    fn merge_partition_columns(&mut self, plan: &mut Plan, parent: u32) -> Result<()> {
        let rel = self
            .catalog
            .relation(parent)
            .ok_or_else(|| Error::internal(format!("cache lookup failed for relation {parent}")))?;
        let mut columns = Vec::with_capacity(rel.columns.len());
        for (i, c) in rel.columns.iter().enumerate().filter(|(_, c)| !c.dropped) {
            let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
            let default = c
                .has_default
                .then(|| self.catalog.default_of(parent, attnum))
                .flatten()
                .map(|d| RawDefault::Cooked(d.expr.clone(), parent));
            let mut inherited = c.clone();
            inherited.local = false;
            inherited.inhcount = 1;
            columns.push(NewColumn {
                name: c.name.clone(),
                type_name: TypeName::default(),
                collate: None,
                collation: c.collation,
                is_not_null: false,
                default,
                identity: 0,
                generated: c.generated,
                storage: None,
                compression: None,
                inherited: Some(inherited),
            });
        }
        for local in std::mem::take(&mut plan.columns) {
            let Some(column) = columns.iter_mut().find(|c| c.name == local.name) else {
                return Err(Error::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!("column \"{}\" does not exist", local.name),
                ));
            };
            column.is_not_null |= local.is_not_null;
            if local.default.is_some() {
                column.default = local.default;
            }
        }
        plan.columns = columns;
        Ok(())
    }

    /// `ADD COLUMN` of `ALTER TABLE`: `transformColumnDefinition` and the sequence of a `serial` column or of an identity column, then `ATExecAddColumn`. The column gets its default, its not-null constraint and the `OWNED BY` of its sequence. The result is the other constraints of the column, the unique constraints, the check constraints and the foreign keys, for the later passes, or `None` when the table has the column and the statement has `IF NOT EXISTS`.
    pub(super) fn add_column(
        &mut self,
        target: &Target,
        def: &ColumnDef,
        if_not_exists: bool,
    ) -> Result<Option<Vec<Constraint>>> {
        let mut plan = Plan {
            relname: target.name.clone(),
            namespace: target.namespace,
            columns: Vec::new(),
            checks: Vec::new(),
            not_nulls: Vec::new(),
            indexes: Vec::new(),
            foreign: Vec::new(),
            serials: Vec::new(),
            has_pkey: false,
            partspec: None,
            partbound: None,
            parent: None,
            inherits: Vec::new(),
            typed: false,
        };
        self.column_definition(&mut plan, def)?;
        let Some(column) = plan.columns.first() else {
            return Err(Error::internal("ADD COLUMN without a column"));
        };
        if column.generated != 0 {
            return Err(not_yet("ADD COLUMN of a generated column", place(def.location)));
        }
        // The `OWNED BY` of the sequence of `generateSerialExtraStmts` comes first.
        if target.kind == b'c' && !plan.serials.is_empty() {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("sequence cannot be owned by relation \"{}\"", target.name),
            )
            .with_detail(relkind_detail(target.kind)));
        }
        // `check_for_column_name_collision`. A composite type has no system columns.
        if target.kind != b'c' && SYSTEM_COLUMNS.contains(&column.name.as_str()) {
            return Err(Error::new(
                SqlState::DUPLICATE_COLUMN,
                format!("column name \"{}\" conflicts with a system column name", column.name),
            ));
        }
        let table = target.oid;
        if self.catalog.relation(table).and_then(|r| r.column_number(&column.name)).is_some() {
            let message = format!(
                "column \"{}\" of relation \"{}\" already exists",
                column.name, target.name
            );
            if if_not_exists {
                self.notice(SqlState::DUPLICATE_COLUMN, format!("{message}, skipping"));
                return Ok(None);
            }
            return Err(Error::new(SqlState::DUPLICATE_COLUMN, message));
        }
        let unlogged = self.is_unlogged(target.oid);
        for serial in &mut plan.serials {
            let info = match &serial.identity {
                Some(options) => self.identity_sequence_info(options)?,
                None => serial_info(serial.ty),
            };
            serial.oid = self.new_sequence(serial.namespace, &serial.name, info, unlogged)?;
        }
        let new = self.catalog_column(column)?;
        check_attribute_type(&new.name, new.ty, self.an.env.allow_system_table_mods())?;
        let row_type = self.catalog.relation(table).map_or(0, |r| r.row_type);
        self.check_member_of_itself(new.ty, row_type)?;
        let number = self.catalog.add_column(table, new)?;
        match &column.default {
            Some(RawDefault::Expr(node)) => {
                let name = column.name.clone();
                let node = node.clone();
                self.column_default(target, &name, Some(&node))?;
            }
            Some(RawDefault::Serial(k)) => {
                let expr = nextval(plan.serials[*k].oid)?;
                let refs = references(self.catalog, table, &[&expr])?;
                self.catalog.add_default(table, number, crate::node::write(&expr)?, &refs)?;
            }
            Some(RawDefault::Cooked(..)) | None => {}
        }
        let default = self.catalog.default_of(table, number).map(|d| d.expr.clone());
        self.added.push(Fill { table, column: number, default });
        self.add_not_nulls(table, &plan)?;
        for serial in &plan.serials {
            if serial.identity.is_some() {
                let owner = (table, plan.namespace, plan.relname.as_str());
                self.own_identity_sequence(serial.oid, serial.namespace, owner, number)?;
            } else {
                self.catalog.set_sequence_owner(serial.oid, table, number);
            }
        }
        let mut rest = plan.indexes;
        rest.extend(plan.checks);
        rest.extend(plan.foreign);
        Ok(Some(rest))
    }

    /// `AlterTableCreateToastTable`: a table whose columns can need it gets a TOAST table after its new columns.
    pub(super) fn toast_after_columns(&mut self, table: u32) -> Result<()> {
        let Some(rel) = self.catalog.relation(table) else { return Ok(()) };
        if rel.toast != 0 || rel.kind != RelKind::Table {
            return Ok(());
        }
        let shapes: Vec<Shape> = rel.columns.iter().filter(|c| !c.dropped).map(shape).collect();
        if needs_toast(&shapes) {
            self.catalog.create_toast(table)?;
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
            columns.push(self.catalog_column(column)?);
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
        let shapes: Vec<Shape> = columns.iter().map(shape).collect();
        let scope: Vec<(String, u32, i32)> =
            columns.iter().map(|c| (c.name.clone(), c.ty, c.typmod)).collect();
        let new = NewRelation {
            namespace: plan.namespace,
            name: plan.relname.clone(),
            owner: self.user,
            columns,
        };
        let table = if plan.partspec.is_some() {
            self.catalog.create_partitioned_table(new)?
        } else {
            self.catalog.create_table(new)?
        };
        self.an.add_table(table, &plan.relname, &scope);
        for (i, column) in plan.columns.iter().enumerate() {
            let Some(default) = &column.default else { continue };
            let (ty, typmod) = (scope[i].1, scope[i].2);
            let expr = match default {
                RawDefault::Expr(node) if column.generated != 0 => {
                    self.generation_expression(node, &plan.columns)?
                }
                RawDefault::Expr(node) => {
                    self.an.with_kind(Kind::ColumnDefault, |an| an.transform(Some(node)))?
                }
                RawDefault::Serial(k) => nextval(plan.serials[*k].oid)?,
                RawDefault::Cooked(text, parent) => {
                    crate::node::read(&self.att_map(*parent, table)?.text(text)?)?
                }
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
            if column.generated == 0 && matches!(expr.kind, ExprKind::Const(Value::Null)) {
                continue;
            }
            let refs = references(self.catalog, table, &[&expr])?;
            let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
            self.catalog.add_default(table, attnum, crate::node::write(&expr)?, &refs)?;
        }
        if let (Some(parent), Some(spec)) = (plan.parent, &plan.partbound) {
            let bound = self.partition_bound(&plan.relname, parent, spec, true)?;
            self.catalog.attach_partition(table, parent, bound)?;
            self.inherit_checks(table, &[parent])?;
        }
        if !plan.inherits.is_empty() {
            self.catalog.set_inherits(table, plan.inherits.clone())?;
            self.inherit_checks(table, &plan.inherits)?;
        }
        if let Some(spec) = &plan.partspec {
            let partkey = self.partition_key(table, &plan.relname, spec)?;
            self.catalog.set_partition_key(table, partkey)?;
        }
        self.add_checks(table, &plan.checks)?;
        self.add_not_nulls(table, plan)?;
        if let Some(parent) = plan.parent {
            self.inherit_not_nulls(table, &[parent])?;
        }
        self.inherit_not_nulls(table, &plan.inherits)?;
        if plan.partspec.is_none() && needs_toast(&shapes) {
            self.catalog.create_toast(table)?;
        }
        Ok(table)
    }

    /// The part of `cookDefault` for the generation expression of a generated column of a new table: the expression must not read a generated column or the whole row, as `check_nested_generated` finds, and it must be immutable. `columns` are the columns of the table.
    fn generation_expression(&mut self, node: &Node, columns: &[NewColumn]) -> Result<Expr> {
        let expr = self.an.with_kind(Kind::Generated, |an| an.transform(Some(node)))?;
        let error = expr.find(0, &mut |e, depth| {
            let ExprKind::Var(var) = e.kind else { return None };
            if var.levels_up != depth {
                return None;
            }
            let at = e.location;
            if var.attnum == 0 {
                return Some(
                    Error::new(
                        SqlState::INVALID_OBJECT_DEFINITION,
                        "cannot use whole-row variable in column generation expression",
                    )
                    .with_detail(
                        "This would cause the generated column to depend on its own value.",
                    )
                    .at_opt(at),
                );
            }
            let column = usize::try_from(var.attnum).ok().and_then(|n| columns.get(n - 1))?;
            (column.generated != 0).then(|| {
                Error::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    format!(
                        "cannot use generated column \"{}\" in column generation expression",
                        column.name
                    ),
                )
                .with_detail("A generated column cannot reference another generated column.")
                .at_opt(at)
            })
        });
        if let Some(error) = error {
            return Err(error);
        }
        if is_mutable(&expr) {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                "generation expression is not immutable",
            ));
        }
        Ok(expr)
    }

    /// The check constraints of `AddRelationNewConstraints`. The result is the OIDs of the new constraints.
    pub(super) fn add_checks(&mut self, table: u32, checks: &[Constraint]) -> Result<Vec<u32>> {
        let mut names: Vec<String> = Vec::new();
        let mut added = Vec::new();
        for con in checks {
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
            let refs = references(self.catalog, table, &[&expr])?;
            let text = crate::node::write(&expr)?;
            if let Some(name) = con.conname.as_deref()
                && let Some(oid) = self.merge_check(table, name, &text, con.is_no_inherit)?
            {
                names.push(name.to_string());
                added.push(oid);
                continue;
            }
            if con.is_no_inherit
                && let Some(rel) = self.catalog.relation(table)
                && rel.kind == RelKind::Partitioned
            {
                return Err(Error::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    format!(
                        "cannot add NO INHERIT constraint to partitioned table \"{}\"",
                        rel.name
                    ),
                ));
            }
            let oid = self.catalog.add_check(
                table,
                con.conname.as_deref(),
                text,
                keys,
                &refs,
                con.is_no_inherit,
            )?;
            if let Some(con) = self.catalog.constraint(oid) {
                names.push(con.name.clone());
            }
            added.push(oid);
        }
        Ok(added)
    }

    /// `MergeWithExistingConstraint`: a check constraint of the statement with the name of a constraint that the table takes from a parent is that constraint when the expressions are the same. The result is the OID of the constraint, or `None` when the table has no such constraint.
    fn merge_check(
        &mut self,
        table: u32,
        name: &str,
        text: &str,
        no_inherit: bool,
    ) -> Result<Option<u32>> {
        let Some(con) = self
            .catalog
            .constraints_of(table)
            .find(|c| c.kind == ConKind::Check && c.name == name && !c.local)
        else {
            return Ok(None);
        };
        let expr = con.expr.as_deref();
        if without_locations(expr) != without_locations(Some(text)) {
            return Ok(None);
        }
        let (oid, inhcount) = (con.oid, con.inhcount);
        if no_inherit {
            let relation = self.catalog.relation(table).map(|r| r.name.clone()).unwrap_or_default();
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!(
                    "constraint \"{name}\" conflicts with inherited constraint on relation \"{relation}\""
                ),
            ));
        }
        self.notice(
            SqlState::SUCCESSFUL_COMPLETION,
            format!("merging constraint \"{name}\" with inherited definition"),
        );
        self.catalog.set_constraint_inherited(oid, true, inhcount)?;
        Ok(Some(oid))
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
                    ))
                    .with_object(Object::Schema, "pg_catalog")
                    .with_object(Object::Table, "pg_constraint")
                    .with_object(Object::Constraint, "pg_constraint_conrelid_contypid_conname_index"));
                }
            }
            self.catalog.add_not_null(table, name.as_deref(), attnum, list[i].is_no_inherit)?;
            i += 1;
        }
        Ok(())
    }
}

/// The facts of the type of a column that `needs_toast_table` reads.
fn shape(column: &Column) -> Shape {
    let row = types::row(column.ty);
    Shape {
        len: row.map_or(-1, |t| t.len),
        align: row.map_or(b'i', |t| t.align),
        storage: if column.storage == 0 { row.map_or(b'x', |t| t.storage) } else { column.storage },
        ty: column.ty,
        typmod: column.typmod,
    }
}

/// The error of a not-null constraint `NO INHERIT` of a partitioned table.
fn no_inherit_not_null() -> Error {
    Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "not-null constraints on partitioned tables cannot be NO INHERIT",
    )
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
        ConstrType::CONSTR_NOTNULL => {
            if plan.partspec.is_some() && con.is_no_inherit {
                return Err(no_inherit_not_null());
            }
            plan.not_nulls.push(con);
        }
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
    let mut params: Vec<&str> = Vec::new();
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
        if params.contains(&key) {
            let what = if primary { "primary key" } else { "unique" };
            return Err(Error::new(
                SqlState::DUPLICATE_COLUMN,
                format!("column \"{key}\" appears twice in {what} constraint"),
            )
            .at_opt(at));
        }
        params.push(key);
    }
    let including = names(&con.including);
    for key in &including {
        if !plan.columns.iter().any(|c| c.name == *key) && !SYSTEM_COLUMNS.contains(key) {
            return Err(missing(key));
        }
    }
    Ok(constraint_index(con, rv, &params, &including))
}

/// The index statement of a primary key or a unique constraint on the key columns and the included columns.
pub(super) fn constraint_index(
    con: &Constraint,
    rv: &RangeVar,
    keys: &[&str],
    including: &[&str],
) -> IndexStmt {
    let elems = |list: &[&str]| -> Vec<Option<Node>> {
        list.iter()
            .map(|key| {
                let elem =
                    IndexElem { name: Some((*key).into()), location: -1, ..IndexElem::default() };
                Some(Node::IndexElem(Box::new(elem)))
            })
            .collect()
    };
    IndexStmt {
        idxname: con.conname.clone(),
        relation: Some(Box::new(rv.clone())),
        accessMethod: Some(con.access_method.clone().unwrap_or_else(|| "btree".into())),
        tableSpace: con.indexspace.clone(),
        indexParams: elems(keys),
        indexIncludingParams: elems(including),
        options: con.options.clone(),
        whereClause: con.where_clause.clone(),
        unique: true,
        nulls_not_distinct: con.nulls_not_distinct,
        primary: con.contype == ConstrType::CONSTR_PRIMARY,
        isconstraint: true,
        deferrable: con.deferrable,
        initdeferred: con.initdeferred,
        ..IndexStmt::default()
    }
}

/// `CheckAttributeType`: a column cannot have a pseudo-type, also as the base type of a domain or the element type of an array. The columns of a composite type passed the check when the type was made. `anyarray` allows the type `anyarray`, as `CHKATYPE_ANYARRAY` does when `allow_system_table_mods` is on.
pub(crate) fn check_attribute_type(name: &str, ty: u32, anyarray: bool) -> Result<()> {
    let Some(row) = types::row(ty) else { return Ok(()) };
    match row.kind {
        b'p' if anyarray && ty == oid::ANYARRAY => Ok(()),
        b'p' => Err(Error::new(
            SqlState::INVALID_TABLE_DEFINITION,
            format!("column \"{name}\" has pseudo-type {}", types::name(ty)),
        )),
        b'd' => check_attribute_type(name, row.base, false),
        _ if types::element(ty) != 0 => check_attribute_type(name, types::element(ty), false),
        _ => Ok(()),
    }
}
