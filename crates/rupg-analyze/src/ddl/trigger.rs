//! `CreateTrigger`: `CREATE TRIGGER`, `DROP TRIGGER`, `ALTER TRIGGER ... RENAME`, `COMMENT ON TRIGGER` and `ALTER TABLE ... ENABLE TRIGGER`.
//!
//! The analyzer runs the checks of `CreateTriggerFiringOn` in their order and reads the condition of `WHEN` with the relation of the trigger as `old` and as `new`. The catalog keeps the condition in its stored form, as `tgqual` does, and [`trigger_when`] reads it back for `pg_get_triggerdef`. The executor does not fire the triggers yet, so a write to a table with a trigger that fires gives `0A000`.

use rupg_catalog::{
    NewTrigger, ObjRef, PG_TRIGGER, RelKind, TRIGGER_BEFORE, TRIGGER_DELETE, TRIGGER_INSERT,
    TRIGGER_INSTEAD, TRIGGER_ROW, TRIGGER_TRUNCATE, TRIGGER_UPDATE,
};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{CreateTrigStmt, DropBehavior, DropStmt, Node, RangeVar, RenameStmt};
use rupg_types::oid;

use super::comment::{name_list, not_owner, range_var};
use super::drop::owner_word;
use super::view::expr_references;
use super::{Definer, Found, not_yet, relkind_detail, relname};
use crate::agg::Kind;
use crate::coerce::AtOpt;
use crate::expr::{Expr, ExprKind};
use crate::select::Query;
use crate::typename::names;
use crate::{Analyzer, Env, Params};

/// `TRIGGER_TYPE_AFTER`: the timing of `AFTER`, which has no bit.
const TRIGGER_AFTER: i16 = 0;

/// An error with `ERRCODE_INVALID_OBJECT_DEFINITION`.
fn invalid(message: &str) -> Error {
    Error::new(SqlState::INVALID_OBJECT_DEFINITION, message)
}

/// An error with `ERRCODE_FEATURE_NOT_SUPPORTED`.
fn unsupported(message: &str) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, message)
}

/// The error of `get_trigger_oid` for a trigger that does not exist.
fn no_trigger(trigger: &str, table: &str) -> Error {
    Error::new(
        SqlState::UNDEFINED_OBJECT,
        format!("trigger \"{trigger}\" for table \"{table}\" does not exist"),
    )
}

/// An argument of the function of a trigger as text. The grammar makes each argument a string.
fn arg_text(node: &Node) -> Result<String> {
    match node {
        Node::String(s) => Ok(s.to_string()),
        Node::Integer(n) => Ok(n.to_string()),
        Node::Float(f) => Ok(f.to_string()),
        _ => Err(Error::internal("an argument of a trigger that is not a string")),
    }
}

/// The relations `old` and `new` of the condition of `WHEN` for the relation `oid` with the name `name`.
fn when_scope(an: &mut Analyzer<'_>, oid: u32, name: &str) -> Result<Query> {
    an.rule_scope(oid, name, [true, true], true)?;
    Ok(when_base(an))
}

/// The query that has the relations of the scope of [`when_scope`]. The scope keeps no relations after it.
fn when_base(an: &mut Analyzer<'_>) -> Query {
    Query { relations: std::mem::take(&mut an.scope.relations), ..Query::default() }
}

/// The condition of `WHEN` of the trigger of the catalog with the OID, with the query that has `old` and `new` as its relations, or `None` when the catalog has no such trigger or the trigger has no condition. `pg_get_triggerdef` shows the condition.
pub fn trigger_when(env: &dyn Env, oid: u32) -> Result<Option<(Query, Expr)>> {
    let Some(catalog) = env.catalog() else { return Ok(None) };
    let Some(trigger) = catalog.trigger(oid) else { return Ok(None) };
    let Some(qual) = &trigger.qual else { return Ok(None) };
    let name = catalog.relation(trigger.relation).map_or("", |r| r.name.as_str());
    let mut an = Analyzer::new(env, &Params::default());
    let base = when_scope(&mut an, trigger.relation, name)?;
    Ok(Some((base, crate::node::read(qual)?)))
}

impl Definer<'_, '_> {
    /// `CreateTriggerFiringOn` for a trigger of `CREATE TRIGGER`.
    pub(super) fn create_trigger(&mut self, stmt: &CreateTrigStmt) -> Result<()> {
        let rv = stmt.relation.as_deref().cloned().unwrap_or_default();
        let found = self.lookup_relation(&rv)?;
        let name = relname(&rv).to_string();
        let code = self.kind_of(found);
        let timed = matches!(stmt.timing, TRIGGER_BEFORE | TRIGGER_AFTER);
        let wrong = |what: &str, detail: &str| {
            Error::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is a {what}"))
                .with_detail(detail.to_string())
        };
        match code {
            b'r' | b'p' if !timed => {
                return Err(wrong("table", "Tables cannot have INSTEAD OF triggers."));
            }
            b'p' if stmt.row && !stmt.transitionRels.is_empty() => {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("\"{name}\" is a partitioned table"),
                )
                .with_detail(
                    "ROW triggers with transition tables are not supported on partitioned tables.",
                ));
            }
            b'r' | b'p' => {}
            b'v' if stmt.timing != TRIGGER_INSTEAD && stmt.row => {
                return Err(wrong("view", "Views cannot have row-level BEFORE or AFTER triggers."));
            }
            b'v' if stmt.events & TRIGGER_TRUNCATE != 0 => {
                return Err(wrong("view", "Views cannot have TRUNCATE triggers."));
            }
            b'v' => {}
            b'f' if !timed => {
                return Err(wrong(
                    "foreign table",
                    "Foreign tables cannot have INSTEAD OF triggers.",
                ));
            }
            // A constraint trigger on a foreign table could defer a check of a foreign key.
            b'f' if stmt.isconstraint => {
                return Err(wrong(
                    "foreign table",
                    "Foreign tables cannot have constraint triggers.",
                ));
            }
            b'f' => {}
            _ => {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("relation \"{name}\" cannot have triggers"),
                )
                .with_detail(relkind_detail(code)));
            }
        }
        if self.is_system(found) && !self.an.env.allow_system_table_mods() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{name}\" is a system catalog"),
            ));
        }
        let Found::User(oid) = found else {
            return Err(not_yet("a trigger on a system catalog", None));
        };
        let constraint_relation = match stmt.constrrel.as_deref().filter(|_| stmt.isconstraint) {
            Some(rv) => match self.lookup_relation(rv)? {
                Found::User(oid) => oid,
                Found::Builtin(row) => row.oid,
            },
            None => 0,
        };
        // rupg has no `GRANT`, so the privilege `TRIGGER` is the privilege of the owner.
        let owner = self.catalog.relation(oid).map_or(0, |r| r.owner);
        if !self.is_superuser() && !self.owns(owner) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for {} {name}", owner_word(code)),
            ));
        }
        let kind = if stmt.row { TRIGGER_ROW } else { 0 } | stmt.timing | stmt.events;
        let row = kind & TRIGGER_ROW != 0;
        if row && kind & TRIGGER_TRUNCATE != 0 {
            return Err(unsupported("TRUNCATE FOR EACH ROW triggers are not supported"));
        }
        if kind & TRIGGER_INSTEAD != 0 {
            if !row {
                return Err(unsupported("INSTEAD OF triggers must be FOR EACH ROW"));
            }
            if stmt.whenClause.is_some() {
                return Err(unsupported("INSTEAD OF triggers cannot have WHEN conditions"));
            }
            if !stmt.columns.is_empty() {
                return Err(unsupported("INSTEAD OF triggers cannot have column lists"));
            }
        }
        let partition = self.catalog.relation(oid).is_some_and(|r| r.partition_of.is_some());
        if row && partition && !stmt.transitionRels.is_empty() {
            return Err(unsupported(
                "ROW triggers with transition tables are not supported on partitions",
            ));
        }
        let (old_table, new_table) = self.transition_tables(stmt, kind, code, &name)?;
        let mut qual = None;
        let mut qual_refs = Vec::new();
        if let Some(node) = &stmt.whenClause {
            self.an.rule_scope(oid, &name, [true, true], true)?;
            let expr = self.an.with_kind(Kind::TriggerWhen, |a| a.transform(Some(node)))?;
            let expr = self.an.coerce_to_boolean(expr, "WHEN")?;
            let base = when_base(&mut self.an);
            let generated: Vec<(String, bool)> = self
                .catalog
                .relation(oid)
                .map(|r| r.columns.iter().map(|c| (c.name.clone(), c.generated != 0)).collect())
                .unwrap_or_default();
            check_when_vars(&expr, kind, &generated)?;
            expr_references(self.catalog, &expr, &[&base], &mut qual_refs);
            qual = Some(crate::node::write(&expr)?);
        }
        let function = self.trigger_function(stmt)?;
        let trigname = stmt.trigname.as_deref().unwrap_or_default();
        if let Some(existing) = self.catalog.trigger_by_name(oid, trigname) {
            if !stmt.replace {
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!("trigger \"{trigname}\" for relation \"{name}\" already exists"),
                ));
            }
            if existing.internal || existing.parent != 0 {
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!(
                        "trigger \"{trigname}\" for relation \"{name}\" is an internal or a child trigger"
                    ),
                ));
            }
            // The pending events of a constraint trigger and the facts of its constraint make a replace hard.
            if existing.constraint != 0 {
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!(
                        "trigger \"{trigname}\" for relation \"{name}\" is a constraint trigger"
                    ),
                ));
            }
        }
        let args = stmt.args.iter().flatten().map(arg_text).collect::<Result<Vec<_>>>()?;
        if args.len() > usize::from(i16::MAX.unsigned_abs()) {
            return Err(Error::new(
                SqlState::TOO_MANY_ARGUMENTS,
                format!("triggers cannot have more than {} arguments", i16::MAX),
            ));
        }
        let columns = self.trigger_columns(oid, &name, &names(&stmt.columns))?;
        let new = NewTrigger {
            name: trigname.to_string(),
            relation: oid,
            function,
            kind,
            args,
            columns,
            old_table,
            new_table,
            qual,
            constraint: stmt.isconstraint,
            constraint_relation,
            deferrable: stmt.deferrable,
            deferred: stmt.initdeferred,
            parent: 0,
            enabled: 0,
        };
        let trigger = self.catalog.create_trigger(new, stmt.replace, &qual_refs)?;
        if code == b'p' && row {
            let children: Vec<u32> = self.catalog.partitions(oid).iter().map(|r| r.oid).collect();
            for child in children {
                self.clone_trigger(trigger, child, stmt.replace)?;
            }
        }
        Ok(())
    }

    /// The part of `CreateTriggerFiringOn` that clones a row trigger of a partitioned table to a partition, and on to the partitions of a partitioned partition. The clone has the columns of `UPDATE OF` by name and the condition of `WHEN` with the numbers of the columns of the partition, as `map_partition_varattnos` gives them, but no transition tables.
    ///
    /// # Errors
    ///
    /// `42710` when the partition has a trigger with the name and `replace` is false.
    pub(super) fn clone_trigger(&mut self, parent: u32, child: u32, replace: bool) -> Result<()> {
        let Some(from) = self.catalog.trigger(parent).cloned() else { return Ok(()) };
        let columns = self.cloned_columns(from.relation, child, &from.columns);
        let map = self.att_map(from.relation, child)?;
        let qual = from.qual.as_deref().map(|text| map.text(text)).transpose()?;
        let new = NewTrigger {
            name: from.name.clone(),
            relation: child,
            function: from.function,
            kind: from.kind,
            args: from.args.clone(),
            columns,
            old_table: None,
            new_table: None,
            qual,
            constraint: from.constraint != 0,
            constraint_relation: from.constraint_relation,
            deferrable: from.deferrable,
            deferred: from.deferred,
            parent,
            enabled: from.enabled,
        };
        let trigger = self.catalog.create_trigger(new, replace, &[])?;
        if self.catalog.relation(child).is_some_and(|r| r.kind == RelKind::Partitioned) {
            let children: Vec<u32> = self.catalog.partitions(child).iter().map(|r| r.oid).collect();
            for grandchild in children {
                self.clone_trigger(trigger, grandchild, replace)?;
            }
        }
        Ok(())
    }

    /// `CloneRowTriggersToPartition`: clones each row trigger of `parent` that is not internal to the new partition `child`.
    pub(super) fn clone_triggers(&mut self, parent: u32, child: u32) -> Result<()> {
        let mut triggers: Vec<(String, u32)> = self
            .catalog
            .triggers_of(parent)
            .into_iter()
            .filter(|t| t.kind & TRIGGER_ROW != 0 && !t.internal)
            .map(|t| (t.name.clone(), t.oid))
            .collect();
        triggers.sort();
        for (_, trigger) in triggers {
            self.clone_trigger(trigger, child, false)?;
        }
        Ok(())
    }

    /// `DropClonedTriggersFromPartition`: drops the clones in a partition that `DETACH PARTITION` leaves, and with them the clones in its partitions.
    pub(super) fn drop_cloned_triggers(&mut self, partition: u32) -> Result<()> {
        let clones: Vec<u32> = self
            .catalog
            .triggers_of(partition)
            .into_iter()
            .filter(|t| t.parent != 0 && t.constraint_relation == 0)
            .map(|t| t.oid)
            .collect();
        for &trigger in &clones {
            self.catalog.clear_trigger_parent(trigger);
        }
        let objects: Vec<ObjRef> = clones.iter().map(|&t| ObjRef::new(PG_TRIGGER, t)).collect();
        self.perform(&objects, false)
    }

    /// The attribute numbers in `child` of the columns of `UPDATE OF` in `parent`, matched by name.
    fn cloned_columns(&self, parent: u32, child: u32, columns: &[i16]) -> Vec<i16> {
        let (Some(parent), Some(child)) =
            (self.catalog.relation(parent), self.catalog.relation(child))
        else {
            return Vec::new();
        };
        columns.iter().filter_map(|&num| child.column_number(&parent.column(num)?.name)).collect()
    }

    /// The checks of `REFERENCING` of `CreateTriggerFiringOn`: the names of the transition tables `OLD TABLE` and `NEW TABLE`.
    fn transition_tables(
        &self,
        stmt: &CreateTrigStmt,
        kind: i16,
        code: u8,
        name: &str,
    ) -> Result<(Option<String>, Option<String>)> {
        let (mut old, mut new) = (None, None);
        for node in stmt.transitionRels.iter().flatten() {
            let Node::TriggerTransition(tt) = node else { continue };
            if !tt.isTable {
                return Err(unsupported(
                    "ROW variable naming in the REFERENCING clause is not supported",
                )
                .with_hint("Use OLD TABLE or NEW TABLE for naming transition tables."));
            }
            if code == b'f' {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("\"{name}\" is a foreign table"),
                )
                .with_detail("Triggers on foreign tables cannot have transition tables."));
            }
            if code == b'v' {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("\"{name}\" is a view"),
                )
                .with_detail("Triggers on views cannot have transition tables."));
            }
            if stmt.timing != TRIGGER_AFTER {
                return Err(invalid(
                    "transition table name can only be specified for an AFTER trigger",
                ));
            }
            if kind & TRIGGER_TRUNCATE != 0 {
                return Err(unsupported(
                    "TRUNCATE triggers with transition tables are not supported",
                ));
            }
            let events = [TRIGGER_INSERT, TRIGGER_UPDATE, TRIGGER_DELETE];
            if events.iter().filter(|&&e| kind & e != 0).count() != 1 {
                return Err(unsupported(
                    "transition tables cannot be specified for triggers with more than one event",
                ));
            }
            if !stmt.columns.is_empty() {
                return Err(unsupported(
                    "transition tables cannot be specified for triggers with column lists",
                ));
            }
            let table = tt.name.as_deref().unwrap_or_default().to_string();
            if tt.isNew {
                if kind & (TRIGGER_INSERT | TRIGGER_UPDATE) == 0 {
                    return Err(invalid(
                        "NEW TABLE can only be specified for an INSERT or UPDATE trigger",
                    ));
                }
                if new.is_some() {
                    return Err(invalid("NEW TABLE cannot be specified multiple times"));
                }
                new = Some(table);
            } else {
                if kind & (TRIGGER_DELETE | TRIGGER_UPDATE) == 0 {
                    return Err(invalid(
                        "OLD TABLE can only be specified for a DELETE or UPDATE trigger",
                    ));
                }
                if old.is_some() {
                    return Err(invalid("OLD TABLE cannot be specified multiple times"));
                }
                old = Some(table);
            }
        }
        if old.is_some() && old == new {
            return Err(invalid("OLD TABLE name and NEW TABLE name cannot be the same"));
        }
        Ok((old, new))
    }

    /// `LookupFuncName` with no arguments for the function of a trigger, which must give the type `trigger`.
    fn trigger_function(&self, stmt: &CreateTrigStmt) -> Result<u32> {
        let list = names(&stmt.funcname);
        let (schema, name) = self.an.split_name(&list, None)?;
        let schemas = match schema {
            Some(namespace) => vec![namespace],
            None => self.an.path.clone(),
        };
        let found = schemas.iter().find_map(|&namespace| {
            let user = self
                .catalog
                .functions()
                .find(|f| f.namespace == namespace && f.name == name && f.argtypes.is_empty())
                .map(|f| (f.oid, f.rettype));
            user.or_else(|| {
                builtin::procs_named(name)
                    .find(|p| p.namespace == namespace && p.argtypes.is_empty())
                    .map(|p| (p.oid, p.rettype))
            })
        });
        let Some((function, rettype)) = found else {
            return Err(Error::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("function {}() does not exist", list.join(".")),
            ));
        };
        if rettype != oid::TRIGGER {
            return Err(invalid(&format!("function {} must return type trigger", list.join("."))));
        }
        Ok(function)
    }

    /// The numbers of the columns of `UPDATE OF`, as `attnameAttNum` finds them. A system column or a dropped column is not a column for this list.
    fn trigger_columns(&self, oid: u32, name: &str, list: &[&str]) -> Result<Vec<i16>> {
        let rel = self.catalog.relation(oid);
        let mut columns: Vec<i16> = Vec::with_capacity(list.len());
        for &column in list {
            let number =
                rel.and_then(|r| r.columns.iter().position(|c| !c.dropped && c.name == column));
            let Some(number) = number.and_then(|n| i16::try_from(n + 1).ok()) else {
                return Err(Error::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!("column \"{column}\" of relation \"{name}\" does not exist"),
                ));
            };
            if columns.contains(&number) {
                return Err(Error::new(
                    SqlState::DUPLICATE_COLUMN,
                    format!("column \"{column}\" specified more than once"),
                ));
            }
            columns.push(number);
        }
        Ok(columns)
    }

    /// The relation of `DROP TRIGGER`, `ALTER TRIGGER` or `COMMENT ON TRIGGER` and the trigger with the name, or `None` with `missing_ok` when one of them does not exist. The notice of `does_not_exist_skipping` names the relation as the statement gives it.
    fn find_trigger(
        &mut self,
        rv: &RangeVar,
        trigger: &str,
        missing_ok: bool,
    ) -> Result<Option<(u32, u32)>> {
        let found = match self.lookup_relation(rv) {
            Ok(found) => found,
            Err(error)
                if missing_ok
                    && matches!(
                        error.state(),
                        SqlState::UNDEFINED_TABLE | SqlState::UNDEFINED_SCHEMA
                    ) =>
            {
                let message = error.message().to_string();
                self.notice(SqlState::SUCCESSFUL_COMPLETION, format!("{message}, skipping"));
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let name = relname(rv);
        if matches!(self.kind_of(found), b'i' | b'I') {
            return Err(Error::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is an index")));
        }
        let Found::User(relation) = found else {
            if missing_ok {
                return Ok(None);
            }
            return Err(no_trigger(trigger, name));
        };
        match self.catalog.trigger_by_name(relation, trigger) {
            Some(t) => Ok(Some((relation, t.oid))),
            None if missing_ok => {
                let shown = [rv.schemaname.as_deref(), Some(name)]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(".");
                self.notice(
                    SqlState::SUCCESSFUL_COMPLETION,
                    format!(
                        "trigger \"{trigger}\" for relation \"{shown}\" does not exist, skipping"
                    ),
                );
                Ok(None)
            }
            None => Err(no_trigger(trigger, name)),
        }
    }

    /// The check of `check_object_ownership` for a trigger: the role must own its relation.
    fn check_trigger_owner(&self, relation: u32) -> Result<()> {
        let rel = self.catalog.relation(relation);
        let owner = rel.map_or(0, |r| r.owner);
        if !self.is_superuser() && !self.owns(owner) {
            return Err(not_owner("relation", rel.map_or("", |r| r.name.as_str())));
        }
        Ok(())
    }

    /// `RemoveObjects` for `DROP TRIGGER`.
    pub(super) fn drop_trigger(&mut self, stmt: &DropStmt) -> Result<()> {
        let mut objects = Vec::new();
        for item in stmt.objects.iter().flatten() {
            let list = name_list(Some(item));
            let Some((&trigger, parent)) = list.split_last() else { continue };
            let rv = range_var(parent)?;
            let Some((relation, oid)) = self.find_trigger(&rv, trigger, stmt.missing_ok)? else {
                continue;
            };
            self.check_trigger_owner(relation)?;
            objects.push(ObjRef::new(PG_TRIGGER, oid));
        }
        self.perform(&objects, stmt.behavior == DropBehavior::DROP_CASCADE)
    }

    /// `get_object_address_relobject` for `COMMENT ON TRIGGER`.
    pub(super) fn comment_trigger(&mut self, object: Option<&Node>) -> Result<ObjRef> {
        let list = name_list(object);
        let Some((&trigger, parent)) = list.split_last().filter(|(_, r)| !r.is_empty()) else {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "must specify relation and object name",
            ));
        };
        let rv = range_var(parent)?;
        let Some((relation, oid)) = self.find_trigger(&rv, trigger, false)? else {
            return Err(no_trigger(trigger, relname(&rv)));
        };
        self.check_trigger_owner(relation)?;
        Ok(ObjRef::new(PG_TRIGGER, oid))
    }

    /// `renametrig` with the checks of `RangeVarCallbackForRenameTrigger`.
    pub(super) fn rename_trigger(&mut self, stmt: &RenameStmt, newname: &str) -> Result<()> {
        let rv = stmt.relation.as_deref().cloned().unwrap_or_default();
        let found = self.lookup_relation(&rv)?;
        let name = relname(&rv).to_string();
        let code = self.kind_of(found);
        if !matches!(code, b'r' | b'v' | b'f' | b'p') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("relation \"{name}\" cannot have triggers"),
            )
            .with_detail(relkind_detail(code)));
        }
        let owner = match found {
            Found::User(oid) => self.catalog.relation(oid).map_or(0, |r| r.owner),
            Found::Builtin(row) => row.owner,
        };
        if !self.is_superuser() && !self.owns(owner) {
            return Err(not_owner(owner_word(code), &name));
        }
        if self.is_system(found) && !self.an.env.allow_system_table_mods() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{name}\" is a system catalog"),
            ));
        }
        let subname = stmt.subname.as_deref().unwrap_or_default();
        let Found::User(relation) = found else { return Err(no_trigger(subname, &name)) };
        let Some((oid, parent)) =
            self.catalog.trigger_by_name(relation, subname).map(|t| (t.oid, t.parent))
        else {
            return Err(no_trigger(subname, &name));
        };
        if parent != 0 {
            let table = self.catalog.relation(relation).and_then(|r| r.partition_of.as_ref());
            let table = table.and_then(|p| self.catalog.relation(p.parent));
            return Err(unsupported(&format!(
                "cannot rename trigger \"{subname}\" on table \"{name}\""
            ))
            .with_hint(format!(
                "Rename the trigger on the partitioned table \"{}\" instead.",
                table.map_or("", |r| r.name.as_str())
            )));
        }
        self.rename_trigger_tree(oid, newname)
    }

    /// `renametrig_internal` for a trigger, then `renametrig_partition` for its clones in the partitions.
    fn rename_trigger_tree(&mut self, oid: u32, newname: &str) -> Result<()> {
        if self.catalog.trigger(oid).is_some_and(|t| t.name != newname) {
            self.catalog.rename_trigger(oid, newname)?;
        }
        for child in self.clones_of(oid) {
            self.rename_trigger_tree(child, newname)?;
        }
        Ok(())
    }

    /// The triggers of the partitions that are clones of the trigger.
    fn clones_of(&self, oid: u32) -> Vec<u32> {
        let mut clones: Vec<(u32, u32)> = self
            .catalog
            .triggers()
            .filter(|t| t.parent == oid)
            .map(|t| (t.relation, t.oid))
            .collect();
        clones.sort_unstable();
        clones.into_iter().map(|(_, t)| t).collect()
    }

    /// `EnableDisableTrigger`: sets `tgenabled` of the trigger with the name, or of all the triggers of the table with no name. With `skip_system`, the internal triggers keep their state; without it, only a superuser can change an internal trigger. With `recurse`, a row trigger of a partitioned table gives its state to its clones.
    pub(super) fn enable_trigger(
        &mut self,
        table: u32,
        trigger: Option<&str>,
        (enabled, skip_system): (u8, bool),
        recurse: bool,
    ) -> Result<()> {
        let partitioned =
            self.catalog.relation(table).is_some_and(|r| r.kind == RelKind::Partitioned);
        let list: Vec<(u32, bool, String)> = self
            .catalog
            .triggers_of(table)
            .into_iter()
            .filter(|t| trigger.is_none_or(|n| t.name == n))
            .map(|t| (t.oid, t.internal, t.name.clone()))
            .collect();
        let mut found = false;
        for (oid, internal, name) in list {
            if internal {
                if skip_system {
                    continue;
                }
                if !self.is_superuser() {
                    return Err(Error::new(
                        SqlState::INSUFFICIENT_PRIVILEGE,
                        format!("permission denied: \"{name}\" is a system trigger"),
                    ));
                }
            }
            found = true;
            self.catalog.set_trigger_enabled(oid, enabled);
            let row = self.catalog.trigger(oid).is_some_and(|t| t.kind & TRIGGER_ROW != 0);
            if recurse && partitioned && row {
                self.enable_clones(oid, (enabled, skip_system))?;
            }
        }
        if let Some(trigger) = trigger
            && !found
        {
            let name = self.catalog.relation(table).map_or("", |r| r.name.as_str());
            return Err(no_trigger(trigger, name));
        }
        Ok(())
    }
}

impl Definer<'_, '_> {
    /// The recursion of `EnableDisableTrigger` into the partitions, which finds the clones by `tgparentid` and not by name.
    fn enable_clones(&mut self, parent: u32, (enabled, skip_system): (u8, bool)) -> Result<()> {
        for child in self.clones_of(parent) {
            let Some(trigger) = self.catalog.trigger(child) else { continue };
            if trigger.internal && skip_system {
                continue;
            }
            self.catalog.set_trigger_enabled(child, enabled);
            self.enable_clones(child, (enabled, skip_system))?;
        }
        Ok(())
    }
}

/// The checks of `CreateTriggerFiringOn` for the columns that the condition of `WHEN` reads. The relation 0 is `old` and the relation 1 is `new`. `columns` has the name of each column of the table and true for a generated column.
fn check_when_vars(expr: &Expr, kind: i16, columns: &[(String, bool)]) -> Result<()> {
    let row = kind & TRIGGER_ROW != 0;
    let before = kind & TRIGGER_BEFORE != 0;
    let has_generated = columns.iter().any(|(_, generated)| *generated);
    let error = expr.find(0, &mut |e, depth| {
        let ExprKind::Var(var) = &e.kind else { return None };
        if var.levels_up != depth {
            return None;
        }
        let at = e.location;
        let message = match var.relation {
            _ if !row => {
                Some(invalid("statement trigger's WHEN condition cannot reference column values"))
            }
            0 if kind & TRIGGER_INSERT != 0 => {
                Some(invalid("INSERT trigger's WHEN condition cannot reference OLD values"))
            }
            1 if kind & TRIGGER_DELETE != 0 => {
                Some(invalid("DELETE trigger's WHEN condition cannot reference NEW values"))
            }
            1 if var.attnum < 0 && before => Some(unsupported(
                "BEFORE trigger's WHEN condition cannot reference NEW system columns",
            )),
            1 if var.attnum == 0 && before && has_generated => Some(
                invalid("BEFORE trigger's WHEN condition cannot reference NEW generated columns")
                    .with_detail(
                        "A whole-row reference is used and the table contains generated columns.",
                    ),
            ),
            1 if before => {
                let column = usize::try_from(var.attnum - 1).ok().and_then(|i| columns.get(i));
                column.filter(|(_, generated)| *generated).map(|(name, _)| {
                    invalid(
                        "BEFORE trigger's WHEN condition cannot reference NEW generated columns",
                    )
                    .with_detail(format!("Column \"{name}\" is a generated column."))
                })
            }
            _ => None,
        };
        message.map(|m| m.at_opt(at))
    });
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}
