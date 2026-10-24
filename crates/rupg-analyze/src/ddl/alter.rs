//! `ALTER TABLE` and the other forms of `AlterTableStmt`, as `AlterTable`, `ATPrepCmd` and `ATRewriteCatalogs` run them, and `ALTER SCHEMA ... OWNER TO`, as `AlterSchemaOwner` runs it. The subcommands are `ADD COLUMN`, `ALTER COLUMN ... TYPE`, `SET NOT NULL`, `DROP NOT NULL`, `SET DEFAULT`, `DROP DEFAULT`, `ADD CONSTRAINT`, `DROP CONSTRAINT`, `OWNER TO`, `ADD IDENTITY`, `SET` and `DROP IDENTITY` of an identity column, and `SET STATISTICS`, `SET STORAGE`, `SET COMPRESSION` and the `SET` and `RESET` of the options of a column, the `SET` and `RESET` of the options of the relation, `CLUSTER ON`, `SET WITHOUT CLUSTER`, `REPLICA IDENTITY`, and `SET WITHOUT OIDS`. `ATTACH PARTITION` and `DETACH PARTITION` are in `partition_attach`. The other subcommands give `0A000`.
//!
//! A subcommand on a partitioned table goes on to its partitions, as `partition_alter` describes. `SET DEFAULT`, `DROP DEFAULT`, `SET STATISTICS`, `SET STORAGE` and `ALTER COLUMN ... TYPE` get their checks for each partition, and they run on each partition after they run on the table.
//!
//! All subcommands get their checks first, in the order of the statement. Then they run in the passes of `AlterTablePass`: the drops, the changes of the types of columns, the new columns, the indexes and the constraints on the changed columns, the transform of the new constraints, the not-null constraints, the indexes, the other constraints, the defaults and the new identity columns, and last the change of the owner, the `SET` options of the identity columns and the other subcommands of `AT_PASS_MISC`, in the order of the statement.

use rupg_catalog::{ConKind, DepKind, PG_CLASS, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named, Owned};
use rupg_sql::nodes::{
    AlterOwnerStmt, AlterTableCmd, AlterTableStmt, AlterTableType, ColumnDef, ConstrType,
    Constraint, DropBehavior, IndexStmt, Node, ObjectType, RangeVar, ReplicaIdentityStmt, RoleSpec,
    RoleSpecType, TypeName,
};
use rupg_types::Value;

use super::alter_column::ColumnCmd;
use super::alter_type::{Changed, TypeCmd};
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
const TABLE_TYPE: &[u8] = b"rpfc";
const STATISTICS: &[u8] = b"rpmfiI";
const STORAGE: &[u8] = b"rpmf";
const COMPRESSION: &[u8] = b"rpm";
const REL_OPTIONS: &[u8] = b"rpvmi";
const TABLE_MATVIEW: &[u8] = b"rpm";
const CLUSTER: &[u8] = b"rm";
const INHERIT: &[u8] = b"rf";

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

/// The work of the subcommands on one table after their checks, by pass.
#[derive(Default)]
struct Passes {
    /// The subcommands go on to the partitions of the table: the statement has no `ONLY`, and the table is the table of the statement.
    recurse: bool,
    /// The table is a partition that the recursion of a subcommand reached.
    recursing: bool,
    /// `AT_PASS_DROP`: `DROP DEFAULT`, `DROP NOT NULL`, `DROP CONSTRAINT` and `DROP IDENTITY`, in the order of the statement.
    drops: Vec<DropCmd>,
    /// `AT_PASS_ALTER_TYPE`: `ALTER COLUMN ... TYPE`, in the order of the statement.
    types: Vec<TypeCmd>,
    /// `AT_PASS_ADD_COL`: `ADD COLUMN` with its `IF NOT EXISTS`, in the order of the statement.
    columns: Vec<(ColumnDef, bool)>,
    /// `AT_PASS_SET_EXPRESSION`: `SET EXPRESSION` of the column, with the expression, in the order of the statement.
    expressions: Vec<(String, Node)>,
    /// `AT_PASS_ADD_CONSTR`: the constraints of `ADD CONSTRAINT` before their transform, with the subcommand that gives them.
    constraints: Vec<(Constraint, Origin)>,
    /// `AT_PASS_ADD_OTHERCONSTR`: `SET DEFAULT` and `ADD IDENTITY`, in the order of the statement. The check constraints and the foreign keys come after them, from the transform.
    defaults: Vec<AddCmd>,
    /// `AT_PASS_MISC`: `OWNER TO`, the `SET` options of an identity column, the subcommands that change the statistics, the storage and the options of a column, and the `SET` and `RESET` of the options of the relation, in the order of the statement.
    misc: Vec<MiscCmd>,
    /// `chgPersistence` of `SET LOGGED` and `SET UNLOGGED`: true when the relation becomes unlogged. A subcommand that does not change the persistence leaves it `None`.
    persistence: Option<bool>,
}

/// A subcommand of `AT_PASS_DROP`.
enum DropCmd {
    /// `DROP DEFAULT` of the column.
    Default(String),
    /// `DROP EXPRESSION` of the column, with its `IF EXISTS`.
    Expression(String, bool),
    /// `DROP IDENTITY` of the column, with `IF EXISTS`.
    Identity(String, bool),
    /// `DROP NOT NULL` of the column.
    NotNull(String),
    /// `DROP CONSTRAINT` with the name, `IF EXISTS` and `CASCADE`.
    Constraint(String, bool, bool),
    /// `DROP COLUMN` with the name, `IF EXISTS` and `CASCADE`.
    Column(String, bool, bool),
}

/// A subcommand of `AT_PASS_ADD_OTHERCONSTR`.
enum AddCmd {
    /// `SET DEFAULT` of the column, with the expression.
    Default(String, Node),
    /// `ADD GENERATED ... AS IDENTITY` of the column.
    Identity(String, Box<Constraint>),
}

/// A subcommand of `AT_PASS_MISC`.
enum MiscCmd {
    /// `OWNER TO` with the new owner.
    Owner(RoleSpec),
    /// `SET GENERATED` and the options of the sequence of the column.
    Identity(String, Vec<Option<Node>>),
    /// `SET STATISTICS`, `SET STORAGE`, `SET COMPRESSION`, or `SET` or `RESET` of the options of a column.
    Column(ColumnCmd),
    /// `SET (...)` of the options of the relation, or `RESET (...)` when the flag is true.
    Options(Vec<Option<Node>>, bool),
    /// `CLUSTER ON` with the name of the index, or `SET WITHOUT CLUSTER` with no name.
    Cluster(Option<String>),
    /// `REPLICA IDENTITY`.
    ReplicaIdentity(ReplicaIdentityStmt),
    /// `ENABLE TRIGGER` and `DISABLE TRIGGER`: the name of the trigger or no name for `ALL` and `USER`, the new `tgenabled`, and true for `USER`, which skips the internal triggers.
    Trigger(Option<String>, u8, bool),
    /// `INHERIT`, with the command, which names the new parent.
    Inherit(Box<AlterTableCmd>),
    /// `NO INHERIT` with the parent.
    NoInherit(Box<RangeVar>),
    /// `OF` with the type, or `NOT OF` with no type.
    Of(Option<Box<TypeName>>),
    /// `VALIDATE CONSTRAINT` with the name of the constraint.
    Validate(String),
}

/// The subcommand that gives a constraint of `AT_PASS_ADD_CONSTR`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// `ALTER COLUMN ... SET NOT NULL`, which `ATExecSetNotNull` runs.
    SetNotNull,
    /// `ADD CONSTRAINT` or a constraint of `ADD COLUMN`, which `ATAddCheckNNConstraint` runs.
    Command,
    /// The not-null constraint of a column of a new primary key, which goes on to the partitions even with `ONLY`.
    PrimaryKey,
}

/// A constraint of `AT_PASS_ADD_OTHERCONSTR`.
enum Other {
    Check(Constraint),
    /// A foreign key, and true when `RI_Initial_Check` reads the rows. `transformAlterTableStmt` skips the check for the foreign key of a new column with no default, as all its values are null.
    Foreign(Constraint, bool),
}

/// The relation of an `ALTER TABLE` after its lookup, or a partition that the recursion of a subcommand reaches.
pub(super) struct Target {
    pub(super) oid: u32,
    pub(super) kind: u8,
    pub(super) name: String,
    pub(super) namespace: u32,
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
        if let [Some(Node::AlterTableCmd(cmd))] = stmt.cmds.as_slice()
            && matches!(
                cmd.subtype,
                AlterTableType::AT_AttachPartition
                    | AlterTableType::AT_DetachPartition
                    | AlterTableType::AT_DetachPartitionFinalize
            )
        {
            return self.partition_command(&target, cmd);
        }
        // The work queue of `ATController`: the table of the statement first, then the partitions that `ATSimpleRecursion` and `ATPrepAlterColumnType` reach, in the order that they reach them.
        let passes = Passes { recurse: rv.inh, ..Passes::default() };
        let mut queue = vec![(target, passes)];
        for cmd in stmt.cmds.iter().flatten() {
            let Node::AlterTableCmd(cmd) = cmd else { continue };
            let (target, passes) = &mut queue[0];
            let recurse = self.prep_command(target, cmd, passes)?;
            let mut reached = Vec::new();
            if recurse {
                reached.extend(self.all_inheritors(queue[0].0.oid).into_iter().map(|t| (t, false)));
            }
            // `ATTypedTableRecursion`: a change of the columns of a composite type goes on to its typed tables, and from them to their children.
            if queue[0].0.kind == b'c'
                && matches!(
                    cmd.subtype,
                    AlterTableType::AT_AddColumn
                        | AlterTableType::AT_DropColumn
                        | AlterTableType::AT_AlterColumnType
                )
            {
                let typed = self.typed_tables(&queue[0].0, cmd.behavior)?;
                reached.extend(typed.into_iter().map(|t| (t, true)));
            }
            for (child, typed) in reached {
                let at = match queue.iter().position(|(t, _)| t.oid == child.oid) {
                    Some(at) => at,
                    None => {
                        let passes =
                            Passes { recurse: typed, recursing: true, ..Passes::default() };
                        queue.push((child, passes));
                        queue.len() - 1
                    }
                };
                let (target, passes) = &mut queue[at];
                self.prep_command(target, cmd, passes)?;
            }
        }
        for (target, passes) in queue {
            self.run_passes(&target, passes)?;
        }
        Ok(())
    }

    /// `RangeVarCallbackForAlterRelation`: the owner, the system catalogs and the kind that the statement names.
    pub(super) fn alter_target(
        &self,
        objtype: ObjectType,
        found: Found,
        name: &str,
    ) -> Result<Target> {
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
            ObjectType::OBJECT_TYPE if kind != b'c' => return Err(wrong("a composite type")),
            ObjectType::OBJECT_INDEX if kind != b'i' && kind != b'I' => {
                return Err(wrong("an index"));
            }
            _ => {}
        }
        if kind == b'c' && objtype != ObjectType::OBJECT_TYPE {
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
    pub(super) fn simple_permissions(
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

    /// `ATPrepCmd`: the checks of one subcommand, and its place in the passes. The result is true when the subcommand goes on to all the partitions of the table and their partitions, as `ATSimpleRecursion` and `ATPrepAlterColumnType` take it, and the caller then runs the checks for each of them.
    fn prep_command(
        &mut self,
        target: &Target,
        cmd: &AlterTableCmd,
        passes: &mut Passes,
    ) -> Result<bool> {
        let mut recurse = false;
        match cmd.subtype {
            AlterTableType::AT_AddColumn => {
                self.simple_permissions(target, cmd, TABLE_TYPE)?;
                if !passes.recursing {
                    self.check_not_typed(target.oid, "cannot add column to typed table")?;
                }
                let Some(Node::ColumnDef(def)) = &cmd.def else {
                    return Err(Error::internal("ADD COLUMN without a column"));
                };
                passes.columns.push(((**def).clone(), cmd.missing_ok));
            }
            AlterTableType::AT_AlterColumnType => {
                self.simple_permissions(target, cmd, TABLE_TYPE)?;
                if !passes.recursing {
                    let at = match &cmd.def {
                        Some(Node::ColumnDef(def)) => place(def.location),
                        _ => None,
                    };
                    self.check_not_typed(target.oid, "cannot alter column type of typed table")
                        .map_err(|e| e.at_opt(at))?;
                }
                let change = self.prep_alter_type(target, cmd, passes.recursing)?;
                passes.types.push(change);
                recurse = passes.recurse;
                if !passes.recurse
                    && !passes.recursing
                    && !self.catalog.children(target.oid).is_empty()
                {
                    let column = cmd.name.as_deref().unwrap_or_default();
                    return Err(Error::new(
                        SqlState::INVALID_TABLE_DEFINITION,
                        format!(
                            "type of inherited column \"{column}\" must be changed in child tables too"
                        ),
                    ));
                }
            }
            AlterTableType::AT_SetNotNull => {
                self.simple_permissions(target, cmd, TABLE)?;
                let column = cmd.name.as_deref().unwrap_or_default();
                if SYSTEM_COLUMNS.contains(&column) {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!("cannot alter system column \"{column}\""),
                    ));
                }
                passes.constraints.push((make_not_null(column), Origin::SetNotNull));
            }
            AlterTableType::AT_DropNotNull => {
                self.simple_permissions(target, cmd, TABLE)?;
                let column = cmd.name.as_deref().unwrap_or_default().to_string();
                passes.drops.push(DropCmd::NotNull(column));
            }
            AlterTableType::AT_DropConstraint => {
                self.simple_permissions(target, cmd, TABLE)?;
                let name = cmd.name.as_deref().unwrap_or_default().to_string();
                let cascade = cmd.behavior == DropBehavior::DROP_CASCADE;
                passes.drops.push(DropCmd::Constraint(name, cmd.missing_ok, cascade));
            }
            AlterTableType::AT_DropColumn => {
                self.simple_permissions(target, cmd, TABLE_TYPE)?;
                if !passes.recursing {
                    self.check_not_typed(target.oid, "cannot drop column from typed table")?;
                }
                let name = cmd.name.as_deref().unwrap_or_default().to_string();
                let cascade = cmd.behavior == DropBehavior::DROP_CASCADE;
                passes.drops.push(DropCmd::Column(name, cmd.missing_ok, cascade));
            }
            AlterTableType::AT_ColumnDefault => {
                self.simple_permissions(target, cmd, TABLE_VIEW)?;
                let column = cmd.name.as_deref().unwrap_or_default().to_string();
                recurse = passes.recurse;
                match &cmd.def {
                    Some(def) => passes.defaults.push(AddCmd::Default(column, def.clone())),
                    None => passes.drops.push(DropCmd::Default(column)),
                }
            }
            AlterTableType::AT_SetExpression => {
                self.simple_permissions(target, cmd, TABLE)?;
                recurse = passes.recurse;
                let column = cmd.name.as_deref().unwrap_or_default().to_string();
                let Some(def) = &cmd.def else {
                    return Err(Error::internal("SET EXPRESSION without an expression"));
                };
                passes.expressions.push((column, def.clone()));
            }
            AlterTableType::AT_DropExpression => {
                self.simple_permissions(target, cmd, TABLE)?;
                recurse = passes.recurse;
                self.prep_drop_expression(target, cmd, passes.recurse, passes.recursing)?;
                let column = cmd.name.as_deref().unwrap_or_default().to_string();
                passes.drops.push(DropCmd::Expression(column, cmd.missing_ok));
            }
            AlterTableType::AT_AddConstraint => {
                self.simple_permissions(target, cmd, TABLE)?;
                let Some(Node::Constraint(con)) = &cmd.def else {
                    return Err(Error::internal("ADD CONSTRAINT without a constraint"));
                };
                if con.contype == ConstrType::CONSTR_PRIMARY {
                    self.prep_primary_key(target, con, passes)?;
                }
                passes.constraints.push(((**con).clone(), Origin::Command));
            }
            AlterTableType::AT_SetLogged | AlterTableType::AT_SetUnLogged => {
                self.simple_permissions(target, cmd, b"rS")?;
                if passes.persistence.is_some() {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "cannot change persistence setting twice",
                    ));
                }
                let unlogged = cmd.subtype == AlterTableType::AT_SetUnLogged;
                if self.prep_persistence(target, unlogged)? {
                    passes.persistence = Some(unlogged);
                }
            }
            AlterTableType::AT_ChangeOwner => {
                let role = cmd.newowner.as_deref().cloned().unwrap_or_default();
                passes.misc.push(MiscCmd::Owner(role));
            }
            AlterTableType::AT_AddIdentity => {
                self.simple_permissions(target, cmd, TABLE_VIEW)?;
                let Some(Node::Constraint(con)) = &cmd.def else {
                    return Err(Error::internal("ADD IDENTITY without a constraint"));
                };
                let column = cmd.name.as_deref().unwrap_or_default().to_string();
                passes.defaults.push(AddCmd::Identity(column, con.clone()));
            }
            AlterTableType::AT_SetIdentity => {
                self.simple_permissions(target, cmd, TABLE_VIEW)?;
                let options = match &cmd.def {
                    Some(Node::List(list)) => list.clone(),
                    _ => Vec::new(),
                };
                let column = cmd.name.as_deref().unwrap_or_default().to_string();
                passes.misc.push(MiscCmd::Identity(column, options));
            }
            AlterTableType::AT_DropIdentity => {
                self.simple_permissions(target, cmd, TABLE_VIEW)?;
                let column = cmd.name.as_deref().unwrap_or_default().to_string();
                passes.drops.push(DropCmd::Identity(column, cmd.missing_ok));
            }
            AlterTableType::AT_SetStatistics => {
                self.simple_permissions(target, cmd, STATISTICS)?;
                let value = match &cmd.def {
                    Some(Node::Integer(n)) => Some(*n),
                    _ => None,
                };
                let name = cmd.name.as_deref().map(str::to_string);
                passes.misc.push(MiscCmd::Column(ColumnCmd::Statistics(name, cmd.num, value)));
                recurse = passes.recurse;
            }
            AlterTableType::AT_SetStorage | AlterTableType::AT_SetCompression => {
                let storage = cmd.subtype == AlterTableType::AT_SetStorage;
                self.simple_permissions(target, cmd, if storage { STORAGE } else { COMPRESSION })?;
                recurse = storage && passes.recurse;
                let column = cmd.name.as_deref().unwrap_or_default().to_string();
                let name = match &cmd.def {
                    Some(Node::String(name)) => name.to_string(),
                    _ => "default".to_string(),
                };
                passes.misc.push(MiscCmd::Column(if storage {
                    ColumnCmd::Storage(column, name)
                } else {
                    ColumnCmd::Compression(column, name)
                }));
            }
            AlterTableType::AT_SetOptions | AlterTableType::AT_ResetOptions => {
                self.simple_permissions(target, cmd, STORAGE)?;
                let column = cmd.name.as_deref().unwrap_or_default().to_string();
                let defs = match &cmd.def {
                    Some(Node::List(list)) => list.clone(),
                    _ => Vec::new(),
                };
                let reset = cmd.subtype == AlterTableType::AT_ResetOptions;
                passes.misc.push(MiscCmd::Column(ColumnCmd::Options(column, defs, reset)));
            }
            AlterTableType::AT_SetRelOptions | AlterTableType::AT_ResetRelOptions => {
                self.simple_permissions(target, cmd, REL_OPTIONS)?;
                let defs = match &cmd.def {
                    Some(Node::List(list)) => list.clone(),
                    _ => Vec::new(),
                };
                let reset = cmd.subtype == AlterTableType::AT_ResetRelOptions;
                passes.misc.push(MiscCmd::Options(defs, reset));
            }
            AlterTableType::AT_ClusterOn | AlterTableType::AT_DropCluster => {
                self.simple_permissions(target, cmd, CLUSTER)?;
                let name =
                    cmd.name.as_deref().filter(|_| cmd.subtype == AlterTableType::AT_ClusterOn);
                passes.misc.push(MiscCmd::Cluster(name.map(str::to_owned)));
            }
            AlterTableType::AT_ReplicaIdentity => {
                self.simple_permissions(target, cmd, TABLE_MATVIEW)?;
                let Some(Node::ReplicaIdentityStmt(stmt)) = &cmd.def else {
                    return Err(Error::internal("REPLICA IDENTITY without a statement"));
                };
                passes.misc.push(MiscCmd::ReplicaIdentity((**stmt).clone()));
            }
            AlterTableType::AT_EnableTrig
            | AlterTableType::AT_EnableAlwaysTrig
            | AlterTableType::AT_EnableReplicaTrig
            | AlterTableType::AT_DisableTrig
            | AlterTableType::AT_EnableTrigAll
            | AlterTableType::AT_DisableTrigAll
            | AlterTableType::AT_EnableTrigUser
            | AlterTableType::AT_DisableTrigUser => {
                self.simple_permissions(target, cmd, TABLE)?;
                let (enabled, named, user) = match cmd.subtype {
                    AlterTableType::AT_EnableTrig => (b'O', true, false),
                    AlterTableType::AT_EnableAlwaysTrig => (b'A', true, false),
                    AlterTableType::AT_EnableReplicaTrig => (b'R', true, false),
                    AlterTableType::AT_DisableTrig => (b'D', true, false),
                    AlterTableType::AT_EnableTrigAll => (b'O', false, false),
                    AlterTableType::AT_DisableTrigAll => (b'D', false, false),
                    AlterTableType::AT_EnableTrigUser => (b'O', false, true),
                    _ => (b'D', false, true),
                };
                let name = cmd.name.as_deref().filter(|_| named).map(str::to_owned);
                passes.misc.push(MiscCmd::Trigger(name, enabled, user));
            }
            AlterTableType::AT_AddInherit => {
                self.simple_permissions(target, cmd, INHERIT)?;
                self.prep_inherit(target)?;
                passes.misc.push(MiscCmd::Inherit(Box::new(cmd.clone())));
            }
            AlterTableType::AT_DropInherit => {
                self.simple_permissions(target, cmd, INHERIT)?;
                self.prep_inherit(target)?;
                let Some(Node::RangeVar(rv)) = &cmd.def else {
                    return Err(Error::internal("NO INHERIT with no parent"));
                };
                passes.misc.push(MiscCmd::NoInherit(rv.clone()));
            }
            AlterTableType::AT_AddOf => {
                self.simple_permissions(target, cmd, TABLE)?;
                let Some(Node::TypeName(name)) = &cmd.def else {
                    return Err(Error::internal("OF with no type"));
                };
                passes.misc.push(MiscCmd::Of(Some(name.clone())));
            }
            AlterTableType::AT_DropOf => {
                self.simple_permissions(target, cmd, TABLE)?;
                passes.misc.push(MiscCmd::Of(None));
            }
            AlterTableType::AT_ValidateConstraint => {
                self.simple_permissions(target, cmd, TABLE)?;
                let name = cmd.name.as_deref().unwrap_or_default().to_string();
                passes.misc.push(MiscCmd::Validate(name));
            }
            // A table has no OIDs, so `SET WITHOUT OIDS` has no work.
            AlterTableType::AT_DropOids => self.simple_permissions(target, cmd, TABLE)?,
            other => {
                let words = action_words(other);
                let what = if words.is_empty() { "this ALTER TABLE action" } else { words };
                return Err(not_yet(&format!("ALTER TABLE ... {what}"), None));
            }
        }
        Ok(recurse)
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
            passes.constraints.push((make_not_null(key), Origin::PrimaryKey));
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
    fn run_passes(&mut self, target: &Target, mut passes: Passes) -> Result<()> {
        for cmd in &passes.drops {
            match cmd {
                DropCmd::Default(column) => self.column_default(target, column, None)?,
                DropCmd::Expression(column, missing_ok) => {
                    self.drop_expression(target, column, *missing_ok)?;
                }
                DropCmd::Identity(column, missing_ok) => {
                    self.drop_identity(target, column, *missing_ok, passes.recurse)?;
                }
                DropCmd::NotNull(column) => self.drop_not_null(target, column, passes.recurse)?,
                DropCmd::Constraint(name, missing_ok, cascade) => {
                    self.drop_constraint(target, name, *missing_ok, *cascade, passes.recurse)?;
                }
                DropCmd::Column(name, missing_ok, cascade) => {
                    self.drop_column(target, name, *missing_ok, *cascade, passes.recurse)?;
                }
            }
        }
        let original = match self.catalog.relation(target.oid) {
            Some(rel) if !passes.types.is_empty() => rel.columns.clone(),
            _ => Vec::new(),
        };
        let mut changed = Changed::default();
        for cmd in &passes.types {
            self.exec_alter_type(target, cmd, &original, &mut changed)?;
        }
        let mut column_constraints = Vec::new();
        let partition = self.catalog.relation(target.oid).is_some_and(|r| r.partition_of.is_some());
        for (def, if_not_exists) in &passes.columns {
            if partition && !passes.recursing {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    "cannot add column to a partition",
                ));
            }
            if let Some(rest) = self.add_column(target, def, *if_not_exists)? {
                let name = def.colname.as_deref().unwrap_or_default();
                let rel = self.catalog.relation(target.oid);
                let number = rel.and_then(|r| r.column_number(name)).unwrap_or(0);
                let identity = rel.and_then(|r| r.column(number)).is_some_and(|c| c.identity != 0);
                let defaulted = identity || self.catalog.default_of(target.oid, number).is_some();
                if !self.catalog.children(target.oid).is_empty() {
                    if !passes.recurse {
                        return Err(Error::new(
                            SqlState::INVALID_TABLE_DEFINITION,
                            "column must be added to child tables too",
                        ));
                    }
                    self.add_column_to_partitions(target.oid, number)?;
                }
                column_constraints.extend(rest.into_iter().map(|con| (con, defaulted)));
            }
        }
        if !passes.columns.is_empty() || !passes.types.is_empty() {
            self.toast_after_columns(target.oid)?;
        }
        for (column, expr) in &passes.expressions {
            self.set_expression(target, column, expr, &mut changed)?;
        }
        self.after_alter_type(&original, &changed)?;
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
        let constraints = std::mem::take(&mut passes.constraints)
            .into_iter()
            .map(|(con, origin)| (con, true, origin));
        let column_constraints = column_constraints
            .into_iter()
            .map(|(con, defaulted)| (con, defaulted, Origin::Command));
        for (con, defaulted, origin) in column_constraints.chain(constraints) {
            match con.contype {
                ConstrType::CONSTR_NOTNULL => not_nulls.push((con, origin)),
                ConstrType::CONSTR_PRIMARY | ConstrType::CONSTR_UNIQUE => {
                    indexes.push(alter_index_constraint(&con, &rv)?);
                }
                ConstrType::CONSTR_EXCLUSION => {
                    return Err(not_yet("an exclusion constraint", place(con.location)));
                }
                ConstrType::CONSTR_CHECK => others.push(Other::Check(con)),
                ConstrType::CONSTR_FOREIGN => others.push(Other::Foreign(con, defaulted)),
                ConstrType(n) => {
                    return Err(Error::internal(format!("unrecognized constraint type: {n}")));
                }
            }
        }
        // `ATRewriteTables` scans the table once to write the rows with their new types and to check the new not-null and check constraints. A composite type has no rows.
        let mut verify = !passes.types.is_empty() && target.kind != b'c';
        if verify {
            self.check_row_type_users(target)?;
        }
        for (con, origin) in &not_nulls {
            verify |=
                self.alter_not_null(target, con, *origin, passes.recurse, passes.recursing)?;
        }
        for index in &indexes {
            self.create_index(index, true)?;
        }
        for cmd in &passes.defaults {
            match cmd {
                AddCmd::Default(column, def) => self.column_default(target, column, Some(def))?,
                AddCmd::Identity(column, con) => {
                    self.add_identity(target, column, con, passes.recurse)?;
                }
            }
        }
        for other in others {
            match other {
                Other::Check(con) => {
                    self.table_scope(target.oid);
                    // `ATAddCheckNNConstraint` gives no query text to `AddRelationNewConstraints`, so the errors of the expression have no place.
                    let added = self
                        .add_checks(target.oid, std::slice::from_ref(&con))
                        .map_err(|e| e.with_position(None))?;
                    // A `NOT VALID` constraint does not check the rows that the table has.
                    let valid = con.initially_valid || !con.is_enforced;
                    verify |= con.is_enforced && valid;
                    for oid in added {
                        if !valid {
                            self.catalog.set_constraint_validated(oid, false)?;
                        }
                        self.add_check_to_partitions(target, oid, passes.recurse)?;
                    }
                }
                Other::Foreign(con, validated) => {
                    if !con.initially_valid && con.is_enforced {
                        return Err(not_yet("NOT VALID", place(con.location)));
                    }
                    if target.kind == b'p' && !passes.recurse {
                        let pk = con.pktable.as_deref().map_or("", relname);
                        return Err(Error::new(
                            SqlState::WRONG_OBJECT_TYPE,
                            format!(
                                "cannot use ONLY for foreign key on partitioned table \"{}\" referencing relation \"{pk}\"",
                                target.name
                            ),
                        ));
                    }
                    self.add_foreign_key(target.oid, &con)?;
                    // `RI_Initial_Check` reads the rows of the table, or of each leaf partition of a partitioned table.
                    if con.is_enforced && validated {
                        let leaves = self.leaves(target.oid);
                        self.scans.extend(leaves);
                    }
                }
            }
        }
        // A partitioned table has no rows. Its partitions get their own scans.
        if verify && target.kind != b'p' {
            self.scans.push(target.oid);
        }
        for cmd in &passes.misc {
            match cmd {
                MiscCmd::Owner(role) => {
                    let owner = self.role_oid(role)?;
                    self.change_owner(target.oid, owner, false)?;
                }
                MiscCmd::Identity(column, options) => {
                    self.set_identity(target, column, options, passes.recurse)?;
                }
                MiscCmd::Column(cmd) => self.alter_column_facts(target, cmd)?,
                MiscCmd::Options(defs, reset) => self.set_rel_options(target, defs, *reset)?,
                MiscCmd::Cluster(Some(name)) => self.cluster_on(target, name)?,
                MiscCmd::Cluster(None) => self.drop_cluster(target),
                MiscCmd::ReplicaIdentity(stmt) => self.replica_identity(target, stmt)?,
                MiscCmd::Trigger(name, enabled, user) => {
                    let (enabled, user) = (*enabled, *user);
                    self.enable_trigger(
                        target.oid,
                        name.as_deref(),
                        (enabled, user),
                        passes.recurse,
                    )?;
                }
                MiscCmd::Inherit(cmd) => self.add_inherit(target, cmd)?,
                MiscCmd::NoInherit(rv) => self.drop_inherit(target, rv)?,
                MiscCmd::Of(Some(name)) => self.add_of(target, name)?,
                MiscCmd::Of(None) => self.drop_of(target)?,
                MiscCmd::Validate(name) => {
                    self.validate_constraint(target, name, passes.recurse, passes.recursing)?;
                }
            }
        }
        if let Some(unlogged) = passes.persistence {
            self.catalog.set_unlogged(target.oid, unlogged)?;
        }
        Ok(())
    }

    /// `ATPrepChangePersistence`: false when the relation has the persistence already. A table that becomes logged cannot refer to an unlogged table, and a table that becomes unlogged cannot have a logged table that refers to it.
    fn prep_persistence(&self, target: &Target, unlogged: bool) -> Result<bool> {
        match self.persistence(target.oid) {
            b't' => {
                return Err(Error::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    format!(
                        "cannot change logged status of table \"{}\" because it is temporary",
                        target.name
                    ),
                ));
            }
            b'u' if unlogged => return Ok(false),
            b'p' if !unlogged => return Ok(false),
            _ => {}
        }
        for con in self.catalog.constraints() {
            let Some(fk) = &con.foreign else { continue };
            if con.relation == target.oid && fk.table == target.oid {
                continue;
            }
            let (other, bad) = if unlogged {
                (con.relation, fk.table == target.oid && self.persistence(con.relation) == b'p')
            } else {
                (fk.table, con.relation == target.oid && self.persistence(fk.table) != b'p')
            };
            if bad {
                let other = self.catalog.relation(other).map_or("", |r| r.name.as_str());
                let (to, what) =
                    if unlogged { ("unlogged", "logged") } else { ("logged", "unlogged") };
                return Err(Error::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    format!(
                        "could not change table \"{}\" to {to} because it references {what} table \"{other}\"",
                        target.name
                    ),
                ));
            }
        }
        Ok(true)
    }

    /// The scope of the analyzer with the columns of the table, for the expressions of a default or a check constraint.
    pub(super) fn table_scope(&mut self, table: u32) {
        self.an.scope = crate::from::Scope::default();
        if let Some(rel) = self.catalog.relation(table) {
            let columns: Vec<(String, u32, i32)> =
                rel.columns.iter().map(crate::from::scope_column).collect();
            let relname = rel.name.clone();
            self.an.add_table(table, &relname, &columns);
        }
    }

    /// `ATExecColumnDefault`: drops the default of the column, then adds the new default.
    pub(super) fn column_default(
        &mut self,
        target: &Target,
        column: &str,
        def: Option<&Node>,
    ) -> Result<()> {
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
        let generated = rel.and_then(|r| r.column(number)).map_or(0, |c| c.generated);
        if generated != 0 {
            let error = Error::new(
                SqlState::SYNTAX_ERROR,
                format!(
                    "column \"{column}\" of relation \"{}\" is a generated column",
                    target.name
                ),
            );
            return Err(match def {
                Some(_) => {
                    error.with_hint("Use ALTER TABLE ... ALTER COLUMN ... SET EXPRESSION instead.")
                }
                None if generated == b's' => {
                    error.with_hint("Use ALTER TABLE ... ALTER COLUMN ... DROP EXPRESSION instead.")
                }
                None => error,
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
        let refs = references(self.catalog, target.oid, &[&expr])?;
        self.catalog.add_default(target.oid, number, crate::node::write(&expr)?, &refs)?;
        Ok(())
    }

    /// `ATExecSetNotNull`, and the not-null constraint of `ATAddCheckNNConstraint` and `AddRelationNewConstraints`. A column with a not-null constraint keeps it, as `AdjustNotNullInheritance` does: the constraint counts the parent table when the recursion reaches the table, and else it becomes a constraint of the table itself. A new constraint goes on to the children with the same name. With `ONLY`, a table with children cannot get a new constraint, but `SET NOT NULL` gives a table with children of `INHERITS` a constraint that is `NO INHERIT`, and the not-null constraints of a primary key always go on to the partitions. The result is true for a new constraint, which a scan of the table must check.
    fn alter_not_null(
        &mut self,
        target: &Target,
        con: &Constraint,
        origin: Origin,
        recurse: bool,
        recursing: bool,
    ) -> Result<bool> {
        let recurse = recurse || origin == Origin::PrimaryKey;
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
                && !recursing
            {
                return Err(prerequisite(format!(
                    "cannot create not-null constraint \"{new}\" on column \"{key}\" of table \"{}\"",
                    target.name
                ))
                .with_detail(format!(
                    "A not-null constraint named \"{name}\" already exists for this column."
                )));
            }
            let found = self
                .catalog
                .constraints_of(target.oid)
                .find(|c| c.kind == ConKind::NotNull && c.keys == [column])
                .map(|c| (c.oid, c.local, c.inhcount));
            if let Some((oid, local, inhcount)) = found {
                if recursing {
                    self.catalog.set_constraint_inherited(
                        oid,
                        local,
                        inhcount.saturating_add(1),
                    )?;
                } else if !local {
                    self.catalog.set_constraint_inherited(oid, true, inhcount)?;
                }
            }
            return Ok(false);
        }
        let children = self.child_targets(target.oid);
        let mut no_inherit = con.is_no_inherit;
        if !recurse && !children.is_empty() {
            if origin == Origin::SetNotNull && target.kind != b'p' {
                no_inherit = true;
            } else {
                let error = Error::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "constraint must be added to child tables too",
                );
                return Err(match origin {
                    Origin::SetNotNull => error.with_hint("Do not specify the ONLY keyword."),
                    _ => error,
                });
            }
        }
        let oid =
            self.catalog.add_not_null(target.oid, con.conname.as_deref(), column, no_inherit)?;
        if recursing {
            self.catalog.set_constraint_inherited(oid, false, 1)?;
        }
        if no_inherit {
            return Ok(true);
        }
        let mut inherited = con.clone();
        inherited.conname = self.catalog.constraint(oid).map(|c| c.name.as_str().into());
        for child in children {
            if self.alter_not_null(&child, &inherited, origin, true, true)? && child.kind != b'p' {
                self.scans.push(child.oid);
            }
        }
        Ok(true)
    }

    /// `ATExecChangeOwner`: the new owner of the relation, then of its indexes, its TOAST table and the sequences that it owns. `recursing` is true for these.
    pub(super) fn change_owner(&mut self, oid: u32, new: u32, recursing: bool) -> Result<()> {
        let Some(rel) = self.catalog.relation(oid) else {
            return Err(Error::internal(format!("cache lookup failed for relation {oid}")));
        };
        let (old, name, namespace, toast) = (rel.owner, rel.name.clone(), rel.namespace, rel.toast);
        let kind = self.kind_of(Found::User(oid));
        let mut new = new;
        match kind {
            b'r' | b'v' | b'm' | b'f' | b'p' => {}
            b'i' | b'I' => {
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
            b't' | b'c' if recursing => {}
            b'c' => {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("\"{name}\" is a composite type"),
                )
                .with_hint("Use ALTER TYPE instead."));
            }
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
        self.catalog.set_relation_owner(oid, new, &role_name(old), &role_name(new))?;
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
    pub(super) fn check_can_set_role(&self, role: u32) -> Result<()> {
        if self.is_superuser() || can_set_role(self.user, role) {
            return Ok(());
        }
        Err(Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("must be able to SET ROLE \"{}\"", role_name(role)),
        ))
    }

    /// `ExecAlterOwnerStmt` for a schema, `AlterSchemaOwner`, for a type or a domain, for a function, for an operator, and for an operator class or an operator family.
    pub(super) fn alter_owner(&mut self, stmt: &AlterOwnerStmt) -> Result<()> {
        if matches!(stmt.objectType, ObjectType::OBJECT_TYPE | ObjectType::OBJECT_DOMAIN) {
            return self.alter_type_owner(stmt);
        }
        if super::alter_function::is_function_kind(stmt.objectType) {
            return self.alter_function_owner(stmt);
        }
        if stmt.objectType == ObjectType::OBJECT_OPERATOR {
            return self.alter_operator_owner(stmt);
        }
        if matches!(stmt.objectType, ObjectType::OBJECT_OPCLASS | ObjectType::OBJECT_OPFAMILY) {
            return self.alter_opcf_owner(stmt);
        }
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
        self.catalog.set_schema_owner(oid, new, &role_name(old), &role_name(new))?;
        Ok(())
    }

    /// `get_rolespec_oid`: the OID of the role that a `RoleSpec` names.
    pub(super) fn role_oid(&self, role: &RoleSpec) -> Result<u32> {
        match role.roletype {
            RoleSpecType::ROLESPEC_CSTRING => {
                let name = role.rolename.as_deref().unwrap_or_default();
                rupg_pgcatalog::roles::current().oid(name).ok_or_else(|| {
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

/// The name of a role.
fn role_name(oid: u32) -> String {
    rupg_pgcatalog::roles::current().name(oid).unwrap_or_default().to_string()
}

/// `member_can_set_role` for a role that is not a superuser: the role is the member, or the member reaches it through memberships with the `SET` option.
fn can_set_role(member: u32, role: u32) -> bool {
    let mut reached = vec![member];
    let mut i = 0;
    while i < reached.len() {
        let current = reached[i];
        for m in rupg_pgcatalog::roles::current().edges() {
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
