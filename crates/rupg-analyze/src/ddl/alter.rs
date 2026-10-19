//! `ALTER TABLE` and the other forms of `AlterTableStmt`, as `AlterTable`, `ATPrepCmd` and `ATRewriteCatalogs` run them, and `ALTER SCHEMA ... OWNER TO`, as `AlterSchemaOwner` runs it. The subcommands are `SET DEFAULT`, `DROP DEFAULT`, `ADD CONSTRAINT` and `OWNER TO`. The other subcommands give `0A000`.
//!
//! All subcommands get their checks first, in the order of the statement. Then they run in the passes of `AlterTablePass`: the drops, the transform of the new constraints, the not-null constraints, the indexes, the other constraints and the defaults, and last the change of the owner.

use rupg_catalog::{ConKind, DepKind, PG_CLASS, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named, Owned};
use rupg_sql::nodes::{
    AlterOwnerStmt, AlterTableCmd, AlterTableStmt, AlterTableType, ConstrType, Constraint,
    IndexStmt, Node, ObjectType, RangeVar, RoleSpec, RoleSpecType,
};
use rupg_types::Value;

use super::comment::not_owner;
use super::drop::owner_word;
use super::table::{SYSTEM_COLUMNS, constraint_index, make_not_null, not_null_column};
use super::{Definer, Found, Message, not_yet, references, relkind_detail, relname};
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::expr::ExprKind;
use crate::typename::{names, place};
use crate::types;

/// The relation kinds of `ATSimplePermissions` that a subcommand allows, as `relkind` codes.
const TABLE_VIEW: &[u8] = b"rpvf";
const TABLE: &[u8] = b"rpf";

/// `alter_table_type_to_string`: the words of a subcommand in its errors.
fn action_words(subtype: AlterTableType) -> &'static str {
    const WORDS: [&str; 66] = [
        "ADD COLUMN",
        "ADD COLUMN",
        "ALTER COLUMN ... SET DEFAULT",
        "ALTER COLUMN ... SET DEFAULT",
        "ALTER COLUMN ... DROP NOT NULL",
        "ALTER COLUMN ... SET NOT NULL",
        "ALTER COLUMN ... SET EXPRESSION",
        "ALTER COLUMN ... DROP EXPRESSION",
        "ALTER COLUMN ... SET STATISTICS",
        "ALTER COLUMN ... SET",
        "ALTER COLUMN ... RESET",
        "ALTER COLUMN ... SET STORAGE",
        "ALTER COLUMN ... SET COMPRESSION",
        "DROP COLUMN",
        "",
        "",
        "ADD CONSTRAINT",
        "ADD CONSTRAINT",
        "ADD CONSTRAINT",
        "ALTER CONSTRAINT",
        "VALIDATE CONSTRAINT",
        "ADD CONSTRAINT",
        "DROP CONSTRAINT",
        "",
        "ALTER COLUMN ... SET DATA TYPE",
        "ALTER COLUMN ... OPTIONS",
        "OWNER TO",
        "CLUSTER ON",
        "SET WITHOUT CLUSTER",
        "SET LOGGED",
        "SET UNLOGGED",
        "SET WITHOUT OIDS",
        "SET ACCESS METHOD",
        "SET TABLESPACE",
        "SET",
        "RESET",
        "",
        "ENABLE TRIGGER",
        "ENABLE ALWAYS TRIGGER",
        "ENABLE REPLICA TRIGGER",
        "DISABLE TRIGGER",
        "ENABLE TRIGGER ALL",
        "DISABLE TRIGGER ALL",
        "ENABLE TRIGGER USER",
        "DISABLE TRIGGER USER",
        "ENABLE RULE",
        "ENABLE ALWAYS RULE",
        "ENABLE REPLICA RULE",
        "DISABLE RULE",
        "INHERIT",
        "NO INHERIT",
        "OF",
        "NOT OF",
        "REPLICA IDENTITY",
        "ENABLE ROW SECURITY",
        "DISABLE ROW SECURITY",
        "FORCE ROW SECURITY",
        "NO FORCE ROW SECURITY",
        "OPTIONS",
        "ATTACH PARTITION",
        "DETACH PARTITION",
        "DETACH PARTITION ... FINALIZE",
        "ALTER COLUMN ... ADD IDENTITY",
        "ALTER COLUMN ... SET",
        "ALTER COLUMN ... DROP IDENTITY",
        "",
    ];
    usize::try_from(subtype.0).ok().and_then(|i| WORDS.get(i)).copied().unwrap_or_default()
}

/// The work of the subcommands after their checks, by pass.
#[derive(Default)]
struct Passes {
    /// `AT_PASS_DROP`: the columns of `DROP DEFAULT`.
    drop_defaults: Vec<String>,
    /// `AT_PASS_ADD_CONSTR`: the constraints of `ADD CONSTRAINT` before their transform.
    constraints: Vec<Constraint>,
    /// `AT_PASS_ADD_OTHERCONSTR`: the columns and the expressions of `SET DEFAULT`. The check constraints and the foreign keys come after them, from the transform.
    defaults: Vec<(String, Node)>,
    /// `AT_PASS_MISC`: the new owners of `OWNER TO`.
    owners: Vec<RoleSpec>,
}

/// A constraint of `AT_PASS_ADD_OTHERCONSTR`.
enum Other {
    Check(Constraint),
    Foreign(Constraint),
}

/// The relation of an `ALTER TABLE` after its lookup.
struct Target {
    oid: u32,
    kind: u8,
    name: String,
    namespace: u32,
}

impl Definer<'_, '_> {
    /// `AlterTable` for the statement, after the lookup of `AlterTableLookupRelation`.
    pub(super) fn alter_table(&mut self, stmt: &AlterTableStmt) -> Result<()> {
        let rv = stmt.relation.as_deref().cloned().unwrap_or_default();
        let found = match self.lookup_relation(&rv) {
            Ok(found) => found,
            Err(e)
                if stmt.missing_ok
                    && matches!(
                        e.state(),
                        SqlState::UNDEFINED_TABLE | SqlState::UNDEFINED_SCHEMA
                    ) =>
            {
                self.notice(
                    SqlState::SUCCESSFUL_COMPLETION,
                    format!("relation \"{}\" does not exist, skipping", relname(&rv)),
                );
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let target = self.alter_target(stmt.objtype, found, relname(&rv))?;
        let mut passes = Passes::default();
        for cmd in stmt.cmds.iter().flatten() {
            let Node::AlterTableCmd(cmd) = cmd else { continue };
            self.prep_command(&target, cmd, &mut passes)?;
        }
        self.run_passes(&target, passes)
    }

    /// `RangeVarCallbackForAlterRelation`: the owner, the system catalogs and the kind that the statement names.
    fn alter_target(&self, objtype: ObjectType, found: Found, name: &str) -> Result<Target> {
        let kind = self.kind_of(found);
        let owner = match found {
            Found::User(oid) => self.catalog.relation(oid).map_or(0, |r| r.owner),
            Found::Builtin(row) => row.owner,
        };
        if !self.is_superuser() && owner != self.user {
            return Err(not_owner(owner_word(kind), name));
        }
        if self.is_system(found) && !self.an.env.allow_system_table_mods() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{name}\" is a system catalog"),
            ));
        }
        let wrong = |what: &str| {
            Error::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is not {what}"))
        };
        match objtype {
            ObjectType::OBJECT_SEQUENCE if kind != b'S' => return Err(wrong("a sequence")),
            ObjectType::OBJECT_VIEW if kind != b'v' => return Err(wrong("a view")),
            ObjectType::OBJECT_MATVIEW if kind != b'm' => {
                return Err(wrong("a materialized view"));
            }
            ObjectType::OBJECT_FOREIGN_TABLE if kind != b'f' => {
                return Err(wrong("a foreign table"));
            }
            ObjectType::OBJECT_INDEX if kind != b'i' && kind != b'I' => {
                return Err(wrong("an index"));
            }
            _ => {}
        }
        if kind == b'c' {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is a composite type"),
            )
            .with_hint("Use ALTER TYPE instead."));
        }
        let Found::User(oid) = found else {
            return Err(not_yet("ALTER on a built-in relation", None));
        };
        let namespace = self.catalog.relation(oid).map_or(0, |r| r.namespace);
        Ok(Target { oid, kind, name: name.to_string(), namespace })
    }

    /// `ATSimplePermissions`: the kinds of relations that the subcommand allows. The lookup checked the owner and the system catalogs before.
    fn simple_permissions(
        &self,
        target: &Target,
        cmd: &AlterTableCmd,
        allowed: &[u8],
    ) -> Result<()> {
        if !allowed.contains(&target.kind) {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!(
                    "ALTER action {} cannot be performed on relation \"{}\"",
                    action_words(cmd.subtype),
                    target.name
                ),
            )
            .with_detail(relkind_detail(target.kind)));
        }
        Ok(())
    }

    /// `ATPrepCmd`: the checks of one subcommand, and its place in the passes.
    fn prep_command(
        &mut self,
        target: &Target,
        cmd: &AlterTableCmd,
        passes: &mut Passes,
    ) -> Result<()> {
        match cmd.subtype {
            AlterTableType::AT_ColumnDefault => {
                self.simple_permissions(target, cmd, TABLE_VIEW)?;
                let column = cmd.name.as_deref().unwrap_or_default().to_string();
                match &cmd.def {
                    Some(def) => passes.defaults.push((column, def.clone())),
                    None => passes.drop_defaults.push(column),
                }
            }
            AlterTableType::AT_AddConstraint => {
                self.simple_permissions(target, cmd, TABLE)?;
                let Some(Node::Constraint(con)) = &cmd.def else {
                    return Err(Error::internal("ADD CONSTRAINT without a constraint"));
                };
                if con.contype == ConstrType::CONSTR_PRIMARY {
                    self.prep_primary_key(target, con, passes)?;
                }
                passes.constraints.push((**con).clone());
            }
            AlterTableType::AT_ChangeOwner => {
                let role = cmd.newowner.as_deref().cloned().unwrap_or_default();
                passes.owners.push(role);
            }
            other => {
                let words = action_words(other);
                let what = if words.is_empty() { "this ALTER TABLE action" } else { words };
                return Err(not_yet(&format!("ALTER TABLE ... {what}"), None));
            }
        }
        Ok(())
    }

    /// `ATPrepAddPrimaryKey`: a not-null constraint for each column of the key that has none. A column with a not-null constraint that is `NO INHERIT` or `NOT VALID` cannot be in a primary key.
    fn prep_primary_key(
        &self,
        target: &Target,
        con: &Constraint,
        passes: &mut Passes,
    ) -> Result<()> {
        for key in names(&con.keys) {
            let column = self.catalog.relation(target.oid).and_then(|r| r.column_number(key));
            let existing = column.and_then(|n| self.not_null_of(target.oid, n));
            if let Some((name, no_inherit, validated)) = existing {
                let (mark, hint) = if no_inherit {
                    (
                        "NO INHERIT",
                        "make the existing constraint inheritable using ALTER TABLE ... ALTER CONSTRAINT ... INHERIT",
                    )
                } else if !validated {
                    ("NOT VALID", "validate it using ALTER TABLE ... VALIDATE CONSTRAINT")
                } else {
                    continue;
                };
                return Err(Error::new(
                    SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("cannot create primary key on column \"{key}\""),
                )
                .with_detail(format!(
                    "The constraint \"{name}\" on column \"{key}\" of table \"{}\", marked {mark}, is incompatible with a primary key.",
                    target.name
                ))
                .with_hint(format!("You might need to {hint}.")));
            }
            passes.constraints.push(make_not_null(key));
        }
        Ok(())
    }

    /// The name, `connoinherit` and `convalidated` of the not-null constraint of a column.
    fn not_null_of(&self, table: u32, column: i16) -> Option<(String, bool, bool)> {
        self.catalog
            .constraints_of(table)
            .find(|c| c.kind == ConKind::NotNull && c.keys == [column])
            .map(|c| (c.name.clone(), c.no_inherit, c.validated))
    }

    /// `ATRewriteCatalogs`: the passes in order.
    fn run_passes(&mut self, target: &Target, passes: Passes) -> Result<()> {
        for column in &passes.drop_defaults {
            self.column_default(target, column, None)?;
        }
        let mut not_nulls = Vec::new();
        let mut indexes = Vec::new();
        let mut others: Vec<Other> = Vec::new();
        let rv = RangeVar {
            schemaname: Some(self.schema_name(target.namespace).into()),
            relname: Some(target.name.as_str().into()),
            inh: true,
            location: -1,
            ..RangeVar::default()
        };
        for con in passes.constraints {
            match con.contype {
                ConstrType::CONSTR_NOTNULL => not_nulls.push(con),
                ConstrType::CONSTR_PRIMARY | ConstrType::CONSTR_UNIQUE => {
                    indexes.push(alter_index_constraint(&con, &rv)?);
                }
                ConstrType::CONSTR_EXCLUSION => {
                    return Err(not_yet("an exclusion constraint", place(con.location)));
                }
                ConstrType::CONSTR_CHECK => others.push(Other::Check(con)),
                ConstrType::CONSTR_FOREIGN => others.push(Other::Foreign(con)),
                ConstrType(n) => {
                    return Err(Error::internal(format!("unrecognized constraint type: {n}")));
                }
            }
        }
        // `ATRewriteTables` scans the table once to check the new not-null and check constraints.
        let mut verify = false;
        for con in &not_nulls {
            verify |= self.alter_not_null(target, con)?;
        }
        for index in &indexes {
            self.create_index(index, true)?;
        }
        for (column, def) in &passes.defaults {
            self.column_default(target, column, Some(def))?;
        }
        for other in others {
            match other {
                Other::Check(con) => {
                    if !con.initially_valid && con.is_enforced {
                        return Err(not_yet("NOT VALID", place(con.location)));
                    }
                    self.table_scope(target.oid);
                    // `ATAddCheckNNConstraint` gives no query text to `AddRelationNewConstraints`, so the errors of the expression have no place.
                    self.add_checks(target.oid, std::slice::from_ref(&con))
                        .map_err(|e| e.with_position(None))?;
                    verify |= con.is_enforced;
                }
                Other::Foreign(con) => {
                    if !con.initially_valid && con.is_enforced {
                        return Err(not_yet("NOT VALID", place(con.location)));
                    }
                    self.add_foreign_key(target.oid, &con)?;
                    // `RI_Initial_Check` reads the rows of the table.
                    if con.is_enforced {
                        self.scans.push(target.oid);
                    }
                }
            }
        }
        if verify {
            self.scans.push(target.oid);
        }
        for role in &passes.owners {
            let owner = self.role_oid(role)?;
            self.change_owner(target.oid, owner, false)?;
        }
        Ok(())
    }

    /// The scope of the analyzer with the columns of the table, for the expressions of a default or a check constraint.
    fn table_scope(&mut self, table: u32) {
        self.an.scope = crate::from::Scope::default();
        if let Some(rel) = self.catalog.relation(table) {
            let columns: Vec<(String, u32, i32)> =
                rel.columns.iter().map(|c| (c.name.clone(), c.ty, c.typmod)).collect();
            let relname = rel.name.clone();
            self.an.add_table(table, &relname, &columns);
        }
    }

    /// `ATExecColumnDefault`: drops the default of the column, then adds the new default.
    fn column_default(&mut self, target: &Target, column: &str, def: Option<&Node>) -> Result<()> {
        let rel = self.catalog.relation(target.oid);
        let Some(number) = rel.and_then(|r| r.column_number(column)) else {
            if SYSTEM_COLUMNS.contains(&column) {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cannot alter system column \"{column}\""),
                ));
            }
            return Err(Error::new(
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{column}\" of relation \"{}\" does not exist", target.name),
            ));
        };
        if rel.and_then(|r| r.column(number)).is_some_and(|c| c.identity != 0) {
            let error = Error::new(
                SqlState::SYNTAX_ERROR,
                format!(
                    "column \"{column}\" of relation \"{}\" is an identity column",
                    target.name
                ),
            );
            return Err(match def {
                Some(_) => error,
                None => {
                    error.with_hint("Use ALTER TABLE ... ALTER COLUMN ... DROP IDENTITY instead.")
                }
            });
        }
        let (ty, typmod) = rel.and_then(|r| r.column(number)).map_or((0, -1), |c| (c.ty, c.typmod));
        self.catalog.remove_default(target.oid, number);
        let Some(node) = def else { return Ok(()) };
        self.table_scope(target.oid);
        // `ATExecColumnDefault` gives no query text to `cookDefault`, so the errors of the expression have no place.
        let expr = self
            .an
            .with_kind(Kind::ColumnDefault, |an| an.transform(Some(node)))
            .map_err(|e| e.with_position(None))?;
        let from = expr.ty;
        let Some(expr) = self
            .an
            .coerce_to_target(expr, ty, typmod, Context::Assignment, None)
            .map_err(|e| e.with_position(None))?
        else {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "column \"{column}\" is of type {} but default expression is of type {}",
                    types::name(ty),
                    types::name(from)
                ),
            )
            .with_hint("You will need to rewrite or cast the expression."));
        };
        if matches!(expr.kind, ExprKind::Const(Value::Null)) {
            return Ok(());
        }
        let refs = references(target.oid, &[&expr]);
        self.catalog.add_default(target.oid, number, crate::node::write(&expr)?, &refs)?;
        Ok(())
    }

    /// The not-null constraint of `AddRelationNewConstraints`. A column with a not-null constraint keeps it, as `AdjustNotNullInheritance` does. The result is true for a new constraint, which a scan of the table must check.
    fn alter_not_null(&mut self, target: &Target, con: &Constraint) -> Result<bool> {
        let key = not_null_column(con).to_string();
        let column = self.catalog.relation(target.oid).and_then(|r| r.column_number(&key));
        let Some(column) = column else {
            if SYSTEM_COLUMNS.contains(&key.as_str()) {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cannot add not-null constraint on system column \"{key}\""),
                ));
            }
            return Err(Error::new(
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{key}\" of relation \"{}\" does not exist", target.name),
            ));
        };
        if !con.initially_valid {
            return Err(not_yet("NOT VALID", place(con.location)));
        }
        if let Some((name, no_inherit, validated)) = self.not_null_of(target.oid, column) {
            let prerequisite =
                |message: String| Error::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, message);
            if no_inherit != con.is_no_inherit {
                return Err(prerequisite(format!(
                    "cannot change NO INHERIT status of NOT NULL constraint \"{name}\" on relation \"{}\"",
                    target.name
                ))
                .with_hint(
                    "You might need to make the existing constraint inheritable using ALTER TABLE ... ALTER CONSTRAINT ... INHERIT.",
                ));
            }
            if !validated {
                return Err(prerequisite(format!(
                    "incompatible NOT VALID constraint \"{name}\" on relation \"{}\"",
                    target.name
                ))
                .with_hint(
                    "You might need to validate it using ALTER TABLE ... VALIDATE CONSTRAINT.",
                ));
            }
            if let Some(new) = con.conname.as_deref()
                && new != name
            {
                return Err(prerequisite(format!(
                    "cannot create not-null constraint \"{new}\" on column \"{key}\" of table \"{}\"",
                    target.name
                ))
                .with_detail(format!(
                    "A not-null constraint named \"{name}\" already exists for this column."
                )));
            }
            return Ok(false);
        }
        self.catalog.add_not_null(target.oid, con.conname.as_deref(), column, con.is_no_inherit)?;
        Ok(true)
    }

    /// `ATExecChangeOwner`: the new owner of the relation, then of its indexes, its TOAST table and the sequences that it owns. `recursing` is true for these.
    fn change_owner(&mut self, oid: u32, new: u32, recursing: bool) -> Result<()> {
        let Some(rel) = self.catalog.relation(oid) else {
            return Err(Error::internal(format!("cache lookup failed for relation {oid}")));
        };
        let (old, name, namespace, toast) = (rel.owner, rel.name.clone(), rel.namespace, rel.toast);
        let kind = self.kind_of(Found::User(oid));
        let mut new = new;
        match kind {
            b'r' | b'v' | b'm' | b'f' | b'p' => {}
            b'i' => {
                if !recursing {
                    if old != new {
                        self.flush();
                        self.messages.push(Message::Warning(
                            Error::new(
                                SqlState::WRONG_OBJECT_TYPE,
                                format!("cannot change owner of index \"{name}\""),
                            )
                            .with_hint("Change the ownership of the index's table instead."),
                        ));
                    }
                    new = old;
                }
            }
            b'S' => {
                if !recursing
                    && old != new
                    && let Some(table) = self.sequence_table(oid)
                {
                    let table =
                        self.catalog.relation(table).map_or_else(String::new, |r| r.name.clone());
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!("cannot change owner of sequence \"{name}\""),
                    )
                    .with_detail(format!("Sequence \"{name}\" is linked to table \"{table}\".")));
                }
            }
            b't' if recursing => {}
            _ => {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("cannot change owner of relation \"{name}\""),
                )
                .with_detail(relkind_detail(kind)));
            }
        }
        if old == new {
            return Ok(());
        }
        if !recursing && !self.is_superuser() {
            if old != self.user {
                return Err(not_owner(owner_word(kind), &name));
            }
            self.check_can_set_role(new)?;
            if !self.may_create_in(namespace, new) {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for schema {}", self.schema_name(namespace)),
                ));
            }
        }
        self.catalog.set_relation_owner(oid, new, role_name(old), role_name(new))?;
        let mut parts: Vec<u32> = Vec::new();
        if matches!(kind, b'r' | b'p' | b'm' | b't') {
            parts.extend(
                self.catalog
                    .relations()
                    .filter(|r| r.index.as_ref().is_some_and(|i| i.table == oid))
                    .map(|r| r.oid),
            );
        }
        if toast != 0 {
            parts.push(toast);
        }
        parts.extend(self.owned_sequences(oid));
        for part in parts {
            self.change_owner(part, new, true)?;
        }
        Ok(())
    }

    /// `sequenceIsOwned`: the table of the column that owns the sequence, by a dependency of the kind `a` or `i`.
    fn sequence_table(&self, sequence: u32) -> Option<u32> {
        self.catalog
            .depends()
            .iter()
            .find(|d| {
                d.object.class == PG_CLASS
                    && d.object.oid == sequence
                    && d.object.sub == 0
                    && d.referenced.class == PG_CLASS
                    && matches!(d.kind, DepKind::Auto | DepKind::Internal)
            })
            .map(|d| d.referenced.oid)
    }

    /// The sequences that `change_owner_recurse_to_sequences` finds: the sequences with a dependency of the kind `a` or `i` on a column of the table.
    fn owned_sequences(&self, table: u32) -> Vec<u32> {
        self.catalog
            .depends()
            .iter()
            .filter(|d| {
                d.referenced.class == PG_CLASS
                    && d.referenced.oid == table
                    && d.referenced.sub != 0
                    && d.object.class == PG_CLASS
                    && d.object.sub == 0
                    && matches!(d.kind, DepKind::Auto | DepKind::Internal)
            })
            .map(|d| d.object.oid)
            .filter(|&oid| self.catalog.relation(oid).is_some_and(|r| r.kind == RelKind::Sequence))
            .collect()
    }

    /// `check_can_set_role`: the role of the statement must be able to `SET ROLE` to the role.
    fn check_can_set_role(&self, role: u32) -> Result<()> {
        if self.is_superuser() || can_set_role(self.user, role) {
            return Ok(());
        }
        Err(Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("must be able to SET ROLE \"{}\"", role_name(role)),
        ))
    }

    /// `ExecAlterOwnerStmt` for a schema: `AlterSchemaOwner`.
    pub(super) fn alter_owner(&mut self, stmt: &AlterOwnerStmt) -> Result<()> {
        if stmt.objectType != ObjectType::OBJECT_SCHEMA {
            return Err(not_yet("this form of ALTER OWNER", None));
        }
        let role = stmt.newowner.as_deref().cloned().unwrap_or_default();
        let new = self.role_oid(&role)?;
        let name = match &stmt.object {
            Some(Node::String(name)) => name.to_string(),
            _ => String::new(),
        };
        let Some(schema) = self.catalog.schema_by_name(&name) else {
            let builtin = builtin::named(Named::Namespace).iter().find(|n| n.name == name);
            let Some(row) = builtin else {
                return Err(Error::new(
                    SqlState::UNDEFINED_SCHEMA,
                    format!("schema \"{name}\" does not exist"),
                ));
            };
            let owner = builtin::owned_by_oid(Owned::Namespace, row.oid).map_or(0, |n| n.owner);
            if owner == new {
                return Ok(());
            }
            return Err(not_yet("ALTER SCHEMA OWNER TO for a built-in schema", None));
        };
        let (oid, old) = (schema.oid, schema.owner);
        if old == new {
            return Ok(());
        }
        if !self.is_superuser() {
            if old != self.user {
                return Err(not_owner("schema", &name));
            }
            self.check_can_set_role(new)?;
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for database {}", self.an.env.database()),
            ));
        }
        self.catalog.set_schema_owner(oid, new, role_name(old), role_name(new))?;
        Ok(())
    }

    /// `get_rolespec_oid`: the OID of the role that a `RoleSpec` names.
    pub(super) fn role_oid(&self, role: &RoleSpec) -> Result<u32> {
        match role.roletype {
            RoleSpecType::ROLESPEC_CSTRING => {
                let name = role.rolename.as_deref().unwrap_or_default();
                builtin::roles().iter().find(|r| r.name == name).map(|r| r.oid).ok_or_else(|| {
                    Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("role \"{name}\" does not exist"),
                    )
                })
            }
            RoleSpecType::ROLESPEC_PUBLIC => {
                Err(Error::new(SqlState::UNDEFINED_OBJECT, "role \"public\" does not exist"))
            }
            _ => Ok(self.user),
        }
    }
}

/// The name of a built-in role.
fn role_name(oid: u32) -> &'static str {
    builtin::roles().iter().find(|r| r.oid == oid).map_or("", |r| r.name)
}

/// `member_can_set_role` for a role that is not a superuser: the role is the member, or the member reaches it through memberships with the `SET` option.
fn can_set_role(member: u32, role: u32) -> bool {
    let mut reached = vec![member];
    let mut i = 0;
    while i < reached.len() {
        let current = reached[i];
        for m in builtin::members() {
            if m.member == current && m.set && !reached.contains(&m.role) {
                reached.push(m.role);
            }
        }
        i += 1;
    }
    reached.contains(&role)
}

/// `transformIndexConstraint` for `ALTER TABLE`: the index of a primary key or a unique constraint on the columns of the table. `DefineIndex` finds the columns, so a column that does not exist gives its error.
fn alter_index_constraint(con: &Constraint, rv: &RangeVar) -> Result<IndexStmt> {
    let at = place(con.location);
    if con.indexname.is_some() {
        return Err(not_yet("ADD CONSTRAINT ... USING INDEX", at));
    }
    if con.without_overlaps {
        return Err(not_yet("WITHOUT OVERLAPS", at));
    }
    let primary = con.contype == ConstrType::CONSTR_PRIMARY;
    let keys = names(&con.keys);
    for (i, key) in keys.iter().enumerate() {
        if keys[..i].contains(key) {
            let what = if primary { "primary key" } else { "unique" };
            return Err(Error::new(
                SqlState::DUPLICATE_COLUMN,
                format!("column \"{key}\" appears twice in {what} constraint"),
            )
            .at_opt(at));
        }
    }
    Ok(constraint_index(con, rv, &keys, &names(&con.including)))
}
