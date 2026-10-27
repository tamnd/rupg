//! The statements that define objects: `CREATE TABLE`, `CREATE INDEX`, `CREATE SCHEMA`, `CREATE VIEW`, `CREATE RULE`, `CREATE TRIGGER`, `CREATE DOMAIN`, `CREATE FUNCTION`, `COMMENT`, `DROP`, `ALTER TABLE`, `ALTER SCHEMA ... OWNER TO`, `CREATE TEXT SEARCH`, `ALTER TEXT SEARCH CONFIGURATION`, `CREATE OPERATOR` and `TRUNCATE`. They run as `transformCreateStmt`, `DefineRelation`, `DefineIndex`, `CreateSchemaCommand`, `DefineView`, `DefineRule`, `CreateTrigger`, `DefineDomain`, `CreateFunction`, `CommentObject`, `ExecDropStmt`, `AlterTable`, `AlterSchemaOwner`, the functions of `tsearchcmds.c`, `DefineOperator` and `ExecuteTruncate` run them, and they change a [`Catalog`]. `CREATE FUNCTION` makes a function in SQL. [`lock_table`] runs the checks of `LOCK TABLE` and takes its locks. It finds its relations by name as these statements do.

mod aggregate;
mod alter;
mod alter_column;
mod alter_constraint;
mod alter_function;
mod alter_inherit;
mod alter_type;
mod attmap;
mod basetype;
mod cast;
mod cluster;
mod cmdtag;
mod comment;
mod conversion;
mod copy;
mod create_table_as;
mod domain;
mod drop;
mod drop_constraint;
mod drop_function;
mod drop_named;
mod enum_label;
mod enumtype;
mod fkey;
mod fkey_partition;
mod foreign;
mod function;
mod generated;
mod grant;
mod identity;
mod index;
mod lock;
mod matview;
mod objaddr;
mod opclass;
mod operator;
mod owned;
mod partition;
mod partition_alter;
mod partition_attach;
mod partition_index;
mod policy;
mod rangetype;
mod reindex;
mod relation_options;
mod reloptions;
mod rename;
mod row_type;
mod rule;
mod schema_elements;
mod sequence;
mod set_schema;
mod statistics;
mod table;
#[cfg(test)]
mod tests;
mod trigger;
mod truncate;
mod tsearch;
mod type_change;
mod using_index;
mod vacuum;
mod validate;
mod view;

pub use copy::{
    CopyRelation, CopyTarget, attnums, copy_filter, copy_insert, copy_relation, copy_target,
    rewrite_insert,
};
pub(crate) use drop::quote_identifier;
pub use function::{
    Body, REGRESS_SYMBOLS, Script, SqlBody, find_library, function_body, function_body_for,
    function_script, function_sqlbody, inline_body,
};
pub use grant::acl_roles;
pub(crate) use lock::relkind;
pub use lock::{Allowed, Take, lock_table};
pub use objaddr::object_address;
pub use owned::{acl_dependents, owned_objects, policy_targets};
pub use policy::policy_tree;
pub use reindex::{ReindexOptions, reindex_options};
pub use reloptions::tablespace_options;
pub use rule::{RuleTree, rule_tree};
pub use schema_elements::SchemaElements;
pub(crate) use table::check_attribute_type;
pub use trigger::trigger_when;
pub use vacuum::vacuum_relations;

use std::cell::RefCell;

use rupg_catalog::{Catalog, ObjRef, PG_CLASS, PG_NAMESPACE, PG_PROC, PG_TYPE, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, ClassRow, Named, NamedRow, Owned};
use rupg_sql::nodes::{CreateSchemaStmt, DiscardMode, GrantTargetType, Node, ObjectType, RangeVar};
use rupg_types::{Value, oid};

use crate::coerce::AtOpt;
use crate::expr::{Expr, ExprKind};
use crate::typename::place;
use crate::{Analyzer, Env, PG_CATALOG_NAMESPACE, Params};

/// The OID of the schema `pg_toast`, where a user cannot create a relation.
const PG_TOAST_NAMESPACE: u32 = 99;

/// `BOOTSTRAP_SUPERUSERID`, the owner of the temporary schemas.
const BOOTSTRAP_SUPERUSER: u32 = 10;

/// A message of a statement that defines an object. The client gets the messages before the result.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// A `WARNING`, such as a precision that is too large.
    Warning(Error),
    /// A `NOTICE`, such as an object that `IF NOT EXISTS` skips.
    Notice(Error),
}

/// The result of [`define`].
#[derive(Debug)]
pub struct Defined {
    /// The messages in order. The client gets them also when the statement fails.
    pub messages: Vec<Message>,
    /// The result of the statement. After an error the caller discards the changes of the catalog, but the OIDs that the statement used stay used, as in PostgreSQL.
    pub result: Result<()>,
    /// The tables that the statement read with a sequential scan, once for each scan, as `pgstat_count_heap_scan` counts them: the build of an index, the check of new constraints by `ALTER TABLE` and the check of a new foreign key. The scans count also when the statement fails.
    pub scans: Vec<u32>,
    /// The tables of `TRUNCATE`, whose rows the caller removes after the checks pass.
    pub truncated: Vec<u32>,
    /// The expressions of `USING` of `ALTER COLUMN ... TYPE`, which give the new values of the columns when the caller writes the rows of the table again.
    pub transforms: Vec<Transform>,
    /// The defaults that the new columns of `ADD COLUMN` had when the subcommand ran, for each new column whose default a later subcommand of the same statement changed. The caller writes the rows of the table again with these defaults.
    pub fills: Vec<Fill>,
    /// The table of `CREATE TABLE AS` or the materialized view of `CREATE MATERIALIZED VIEW` or `REFRESH MATERIALIZED VIEW`, which the caller fills with the rows of the query, or `None`. It is `None` also for `WITH NO DATA` and for a table that exists with `IF NOT EXISTS`.
    pub filled: Option<u32>,
    /// The views whose query reads an object that the statement renamed. The catalog keeps the text of a view, so the caller writes the text again from the query that the old names give.
    pub views: Vec<u32>,
    /// The partitions of `ATTACH PARTITION` whose rows the caller checks against their new partition constraints after the statement, as `QueuePartitionConstraintValidation` queues them.
    pub checks: Vec<PartitionCheck>,
    /// The partitions of `DETACH PARTITION` whose keys a foreign key refers to through the parent. Before the statement the caller checks that no row of a referencing table has a key of the partition, as `ATDetachCheckNoForeignKeyRefs` checks it, with the catalog before the statement.
    pub detached: Vec<u32>,
    /// The roles of `DROP OWNED`. The caller removes the grants of membership that these roles made, as `shdepDropOwned` drops the rows of `pg_auth_members` with the role as the grantor.
    pub grantors: Vec<u32>,
    /// The elements of `CREATE SCHEMA`, which the caller runs after it keeps the new schema, or `None`.
    pub schema: Option<SchemaElements>,
    /// The check of `PreventInTransactionBlock` that the statement needs after it found its relations, such as `REINDEX` of a partitioned table, or `None`. The caller runs the check before it keeps the changes, and after an error the messages of the statement go.
    pub top_level: Option<TopLevel>,
    /// The new index of `CREATE INDEX CONCURRENTLY` on a table that is not temporary, or `None`. The first transaction of `DefineIndex` commits the index before the build, so after a failure of the build the caller keeps the index with `indisvalid` and `indisready` false.
    pub concurrent: Option<u32>,
}

/// A statement that cannot run inside a transaction block or in a function, as `PreventInTransactionBlock` checks it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopLevel {
    /// The name of the statement in the error, such as `REINDEX TABLE`.
    pub statement: &'static str,
    /// The context of the error, such as the partitioned table of `REINDEX`.
    pub context: String,
}

/// A partition whose rows must satisfy its partition constraint, one entry of `tab->partition_constraint`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartitionCheck {
    /// The OID of the partition. The caller checks the rows of each of its leaf partitions.
    pub table: u32,
    /// True for the default partition, whose constraint is smaller after a new partition, with the error of `validateDefaultPartitionConstraint` for a row that violates it.
    pub default: bool,
}

/// The expression of `USING` of `ALTER COLUMN ... TYPE` for one column, as `ATPrepAlterColumnType` keeps it in `tab->newvals`.
#[derive(Clone, Debug)]
pub struct Transform {
    /// The OID of the table.
    pub table: u32,
    /// The name of the column.
    pub column: String,
    /// The expression before its transform. It refers to the columns of the table by their names, and it gives the old values of the columns.
    pub expr: Node,
}

/// The default of a new column of `ALTER TABLE ... ADD COLUMN`, as `ATExecAddColumn` keeps it in `tab->newvals`, or the new expression of a generated column of `SET EXPRESSION`. The rows of the table get this default, also when a later subcommand of the statement sets or drops the default of the column.
#[derive(Clone, Debug)]
pub struct Fill {
    /// The OID of the table.
    pub table: u32,
    /// The number of the column.
    pub column: i16,
    /// The text of the default expression, or `None` for a column with no default, which gets a null.
    pub default: Option<String>,
}

/// True for a statement that [`define`] runs.
pub fn is_definition(stmt: &Node) -> bool {
    matches!(
        stmt,
        Node::CreateStmt(_)
            | Node::CreateForeignTableStmt(_)
            | Node::CreateTableAsStmt(_)
            | Node::IndexStmt(_)
            | Node::CreateSchemaStmt(_)
            | Node::CreateDomainStmt(_)
            | Node::CompositeTypeStmt(_)
            | Node::CreateEnumStmt(_)
            | Node::AlterEnumStmt(_)
            | Node::CreateRangeStmt(_)
            | Node::ViewStmt(_)
            | Node::RuleStmt(_)
            | Node::CreatePolicyStmt(_)
            | Node::AlterPolicyStmt(_)
            | Node::CreateTrigStmt(_)
            | Node::CreateFunctionStmt(_)
            | Node::AlterFunctionStmt(_)
            | Node::CommentStmt(_)
            | Node::CreateSeqStmt(_)
            | Node::AlterSeqStmt(_)
            | Node::AlterTSConfigurationStmt(_)
            | Node::AlterOperatorStmt(_)
            | Node::CreateOpFamilyStmt(_)
            | Node::CreateOpClassStmt(_)
            | Node::AlterOpFamilyStmt(_)
            | Node::TruncateStmt(_)
            | Node::RefreshMatViewStmt(_)
            | Node::CreateStatsStmt(_)
            | Node::AlterStatsStmt(_)
            | Node::DropOwnedStmt(_)
            | Node::ReassignOwnedStmt(_)
            | Node::AlterDefaultPrivilegesStmt(_)
            | Node::CreateCastStmt(_)
            | Node::CreateConversionStmt(_)
            | Node::CreateFdwStmt(_)
            | Node::AlterFdwStmt(_)
            | Node::CreateForeignServerStmt(_)
            | Node::AlterForeignServerStmt(_)
            | Node::CreateUserMappingStmt(_)
            | Node::AlterUserMappingStmt(_)
            | Node::DropUserMappingStmt(_)
    ) || matches!(stmt, Node::DefineStmt(define) if tsearch::is_ts_kind(define.kind) || matches!(define.kind, ObjectType::OBJECT_OPERATOR | ObjectType::OBJECT_AGGREGATE | ObjectType::OBJECT_TYPE))
        || matches!(stmt, Node::DropStmt(drop) if is_drop_kind(drop.removeType))
        || matches!(stmt, Node::DiscardStmt(discard) if discard.target == DiscardMode::DISCARD_TEMP)
        || matches!(stmt, Node::AlterTableStmt(alter) if is_alter_kind(alter.objtype) || alter.objtype == ObjectType::OBJECT_TYPE)
        || matches!(stmt, Node::AlterOwnerStmt(alter) if matches!(alter.objectType, ObjectType::OBJECT_SCHEMA | ObjectType::OBJECT_OPERATOR | ObjectType::OBJECT_STATISTIC_EXT | ObjectType::OBJECT_CONVERSION | ObjectType::OBJECT_TSDICTIONARY | ObjectType::OBJECT_TSCONFIGURATION | ObjectType::OBJECT_FDW | ObjectType::OBJECT_FOREIGN_SERVER) || is_opcf_kind(alter.objectType) || type_change::is_type_kind(alter.objectType) || alter_function::is_function_kind(alter.objectType))
        || matches!(stmt, Node::RenameStmt(rename) if rename::is_rename_kind(rename.renameType))
        || matches!(stmt, Node::AlterObjectSchemaStmt(alter) if is_alter_kind(alter.objectType) || matches!(alter.objectType, ObjectType::OBJECT_OPERATOR | ObjectType::OBJECT_STATISTIC_EXT | ObjectType::OBJECT_CONVERSION) || tsearch::is_ts_kind(alter.objectType) || is_opcf_kind(alter.objectType) || type_change::is_type_kind(alter.objectType) || alter_function::is_function_kind(alter.objectType))
        || matches!(stmt, Node::SelectStmt(select) if crate::select_into(select).is_some())
        || matches!(stmt, Node::GrantStmt(grant) if grant.targtype != GrantTargetType::ACL_TARGET_DEFAULTS)
}

/// True for an operator class and an operator family.
fn is_opcf_kind(kind: ObjectType) -> bool {
    matches!(kind, ObjectType::OBJECT_OPCLASS | ObjectType::OBJECT_OPFAMILY)
}

/// True for the kinds of relations of `ALTER TABLE` and of the other statements that change a relation. `ALTER TYPE` of the attributes of a composite type is an `AlterTableStmt` too.
fn is_alter_kind(kind: ObjectType) -> bool {
    matches!(
        kind,
        ObjectType::OBJECT_TABLE
            | ObjectType::OBJECT_VIEW
            | ObjectType::OBJECT_INDEX
            | ObjectType::OBJECT_SEQUENCE
            | ObjectType::OBJECT_MATVIEW
            | ObjectType::OBJECT_FOREIGN_TABLE
    )
}

/// True for the kinds of objects that `DROP` can remove.
fn is_drop_kind(kind: ObjectType) -> bool {
    matches!(
        kind,
        ObjectType::OBJECT_TABLE
            | ObjectType::OBJECT_VIEW
            | ObjectType::OBJECT_MATVIEW
            | ObjectType::OBJECT_INDEX
            | ObjectType::OBJECT_SEQUENCE
            | ObjectType::OBJECT_SCHEMA
            | ObjectType::OBJECT_TYPE
            | ObjectType::OBJECT_DOMAIN
            | ObjectType::OBJECT_RULE
            | ObjectType::OBJECT_POLICY
            | ObjectType::OBJECT_TRIGGER
            | ObjectType::OBJECT_FUNCTION
            | ObjectType::OBJECT_PROCEDURE
            | ObjectType::OBJECT_ROUTINE
            | ObjectType::OBJECT_AGGREGATE
            | ObjectType::OBJECT_TSPARSER
            | ObjectType::OBJECT_TSDICTIONARY
            | ObjectType::OBJECT_TSTEMPLATE
            | ObjectType::OBJECT_TSCONFIGURATION
            | ObjectType::OBJECT_OPERATOR
            | ObjectType::OBJECT_OPCLASS
            | ObjectType::OBJECT_OPFAMILY
            | ObjectType::OBJECT_STATISTIC_EXT
            | ObjectType::OBJECT_FOREIGN_TABLE
            | ObjectType::OBJECT_ACCESS_METHOD
            | ObjectType::OBJECT_CAST
            | ObjectType::OBJECT_COLLATION
            | ObjectType::OBJECT_CONVERSION
            | ObjectType::OBJECT_EXTENSION
            | ObjectType::OBJECT_LANGUAGE
            | ObjectType::OBJECT_FDW
            | ObjectType::OBJECT_FOREIGN_SERVER
            | ObjectType::OBJECT_EVENT_TRIGGER
            | ObjectType::OBJECT_PUBLICATION
    )
}

/// Runs a statement that defines an object, for the role `user`, and adds the object to the catalog. The statement is the `stmt` of a `RawStmt`, and `text` is its text, which the catalog keeps for a view and for a function.
pub fn define(stmt: &Node, text: &str, env: &dyn Env, catalog: &mut Catalog, user: u32) -> Defined {
    let schemas = SchemaEnv::new(env, catalog);
    let mut definer = Definer {
        an: Analyzer::new(&schemas, &Params::default()),
        catalog,
        user,
        messages: Vec::new(),
        scans: Vec::new(),
        truncated: Vec::new(),
        transforms: Vec::new(),
        added: Vec::new(),
        views: Vec::new(),
        checks: Vec::new(),
        detached: Vec::new(),
        grantors: Vec::new(),
        schema: None,
        top_level: None,
        concurrent: None,
    };
    let mut filled = None;
    let result = match stmt {
        Node::CreateStmt(create) => definer.create_table(create),
        Node::CreateForeignTableStmt(create) => definer.create_foreign_table(create),
        Node::CreateTableAsStmt(ctas) => {
            definer.create_table_as(ctas, text).map(|table| filled = table)
        }
        Node::SelectStmt(select) => definer.select_into(select).map(|table| filled = table),
        Node::RefreshMatViewStmt(stmt) => {
            definer.refresh_matview(stmt).map(|matview| filled = matview)
        }
        Node::IndexStmt(index) => definer.create_index(index, false),
        Node::CreateSchemaStmt(schema) => definer.create_schema(schema, text),
        Node::ViewStmt(view) => definer.create_view(view, text),
        Node::RuleStmt(rule) => definer.create_rule(rule, text),
        Node::CreatePolicyStmt(policy) => definer.create_policy(policy, text),
        Node::AlterPolicyStmt(policy) => definer.alter_policy(policy, text),
        Node::CreateTrigStmt(trigger) => definer.create_trigger(trigger),
        Node::CreateDomainStmt(domain) => definer.create_domain(domain).map(|_| ()),
        Node::CompositeTypeStmt(stmt) => definer.create_composite_type(stmt),
        Node::CreateEnumStmt(stmt) => definer.create_enum(stmt),
        Node::AlterEnumStmt(stmt) => definer.alter_enum(stmt),
        Node::CreateRangeStmt(stmt) => definer.create_range(stmt),
        Node::CreateFunctionStmt(function) => definer.create_function(function, text).map(|_| ()),
        Node::AlterFunctionStmt(alter) => definer.alter_function(alter),
        Node::CommentStmt(comment) => definer.comment(comment),
        Node::DropStmt(drop) => definer.drop(drop),
        Node::DiscardStmt(_) => definer.discard_temp(),
        Node::AlterTableStmt(alter) => definer.alter_table(alter),
        Node::AlterOwnerStmt(alter) => definer.alter_owner(alter),
        Node::RenameStmt(rename) => definer.rename(rename),
        Node::AlterObjectSchemaStmt(alter) if type_change::is_type_kind(alter.objectType) => {
            definer.alter_type_schema(alter)
        }
        Node::AlterObjectSchemaStmt(alter)
            if alter_function::is_function_kind(alter.objectType) =>
        {
            definer.alter_function_schema(alter)
        }
        Node::AlterObjectSchemaStmt(alter) if alter.objectType == ObjectType::OBJECT_OPERATOR => {
            definer.alter_operator_schema(alter)
        }
        Node::AlterObjectSchemaStmt(alter) if is_opcf_kind(alter.objectType) => {
            definer.alter_opcf_schema(alter)
        }
        Node::AlterObjectSchemaStmt(alter)
            if alter.objectType == ObjectType::OBJECT_STATISTIC_EXT =>
        {
            definer.alter_stats_schema(alter)
        }
        Node::AlterObjectSchemaStmt(alter) if alter.objectType == ObjectType::OBJECT_CONVERSION => {
            definer.alter_conversion_schema(alter)
        }
        Node::AlterObjectSchemaStmt(alter) if tsearch::is_ts_kind(alter.objectType) => {
            definer.alter_ts_schema(alter)
        }
        Node::AlterObjectSchemaStmt(alter) => definer.alter_relation_schema(alter),
        Node::CreateStatsStmt(create) => definer.create_statistics(create),
        Node::AlterStatsStmt(alter) => definer.alter_statistics(alter),
        Node::CreateSeqStmt(create) => definer.create_sequence(create),
        Node::AlterSeqStmt(alter) => definer.alter_sequence(alter),
        Node::DefineStmt(define) if tsearch::is_ts_kind(define.kind) => definer.define_ts(define),
        Node::DefineStmt(define) if define.kind == ObjectType::OBJECT_OPERATOR => {
            definer.create_operator(define)
        }
        Node::DefineStmt(define) if define.kind == ObjectType::OBJECT_AGGREGATE => {
            definer.create_aggregate(define)
        }
        Node::DefineStmt(define) if define.kind == ObjectType::OBJECT_TYPE => {
            definer.define_type(define)
        }
        Node::CreateCastStmt(stmt) => definer.create_cast(stmt),
        Node::CreateConversionStmt(stmt) => definer.create_conversion(stmt),
        Node::CreateFdwStmt(stmt) => definer.create_fdw(stmt),
        Node::AlterFdwStmt(stmt) => definer.alter_fdw(stmt),
        Node::CreateForeignServerStmt(stmt) => definer.create_server(stmt),
        Node::AlterForeignServerStmt(stmt) => definer.alter_server(stmt),
        Node::CreateUserMappingStmt(stmt) => definer.create_user_mapping(stmt),
        Node::AlterUserMappingStmt(stmt) => definer.alter_user_mapping(stmt),
        Node::DropUserMappingStmt(stmt) => definer.drop_user_mapping(stmt),
        Node::AlterOperatorStmt(alter) => definer.alter_operator(alter),
        Node::CreateOpFamilyStmt(create) => definer.create_opfamily(create),
        Node::CreateOpClassStmt(create) => definer.create_opclass(create),
        Node::AlterOpFamilyStmt(alter) => definer.alter_opfamily(alter),
        Node::AlterTSConfigurationStmt(alter) => definer.alter_ts_config(alter),
        Node::TruncateStmt(truncate) => definer.truncate(truncate),
        Node::GrantStmt(grant) => definer.grant(grant),
        Node::DropOwnedStmt(stmt) => definer.drop_owned(stmt),
        Node::AlterDefaultPrivilegesStmt(stmt) => definer.alter_default_privileges(stmt),
        Node::ReassignOwnedStmt(stmt) => definer.reassign_owned(stmt),
        Node::ReindexStmt(stmt) => definer.reindex(stmt),
        _ => {
            Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "this statement is not supported yet"))
        }
    };
    definer.flush();
    let catalog = &*definer.catalog;
    let fills = definer
        .added
        .into_iter()
        .filter(|fill| {
            let column = catalog.relation(fill.table).and_then(|r| r.column(fill.column));
            let live = column.is_some_and(|c| !c.dropped);
            // A stored generated column computes its values again from the expression that it has now.
            let generated = column.is_some_and(|c| c.generated != 0);
            let now = catalog.default_of(fill.table, fill.column).map(|d| &d.expr);
            live && (generated || now != fill.default.as_ref())
        })
        .collect();
    Defined {
        messages: definer.messages,
        result,
        scans: definer.scans,
        truncated: definer.truncated,
        transforms: definer.transforms,
        fills,
        filled,
        views: definer.views,
        checks: definer.checks,
        detached: definer.detached,
        grantors: definer.grantors,
        schema: definer.schema,
        top_level: definer.top_level,
        concurrent: definer.concurrent,
    }
}

/// The session with the schemas of the catalog, which the analyzer finds by name.
struct SchemaEnv<'e> {
    env: &'e dyn Env,
    schemas: Vec<(String, u32)>,
    /// The relations that the statement added before the last [`Env::command_counter_increment`], which the session does not have: the schema, the name and the OID.
    relations: RefCell<Vec<(u32, String, u32)>>,
}

impl<'e> SchemaEnv<'e> {
    fn new(env: &'e dyn Env, catalog: &Catalog) -> SchemaEnv<'e> {
        let schemas = catalog.schemas().map(|s| (s.name.clone(), s.oid)).collect();
        SchemaEnv { env, schemas, relations: RefCell::new(Vec::new()) }
    }

    /// The OID of a relation that the statement added, for the input `text` of `regclass`, as `regclassin` finds it after `CommandCounterIncrement`.
    fn new_class(&self, text: &str) -> Option<u32> {
        let names = rupg_types::qualified_name_list(text).ok()?;
        let (spaces, name) = match names.as_slice() {
            [name] => (self.search_path(), name),
            [schema, name] => (vec![self.namespace(schema)?], name),
            _ => return None,
        };
        let relations = self.relations.borrow();
        spaces.iter().find_map(|&ns| {
            relations.iter().find(|(n, rel, _)| *n == ns && rel == name).map(|(_, _, oid)| *oid)
        })
    }
}

impl Env for SchemaEnv<'_> {
    fn search_path(&self) -> Vec<u32> {
        self.env.search_path()
    }

    fn namespace(&self, name: &str) -> Option<u32> {
        self.schemas
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, oid)| *oid)
            .or_else(|| self.env.namespace(name))
    }

    fn namespace_usage(&self, namespace: u32) -> bool {
        self.env.namespace_usage(namespace)
    }

    fn database(&self) -> String {
        self.env.database()
    }

    fn input(&self, ty: u32, text: &str, typmod: i32) -> Result<Value> {
        match self.env.input(ty, text, typmod) {
            Err(error) if ty == oid::REGCLASS && error.state() == SqlState::UNDEFINED_TABLE => {
                self.new_class(text).map(Value::Oid).ok_or(error)
            }
            value => value,
        }
    }

    fn catalog(&self) -> Option<&Catalog> {
        self.env.catalog()
    }

    fn allow_system_table_mods(&self) -> bool {
        self.env.allow_system_table_mods()
    }

    fn replica(&self) -> bool {
        self.env.replica()
    }

    fn check_function_bodies(&self) -> bool {
        self.env.check_function_bodies()
    }

    fn sequence_row(&self, file: u32) -> Option<rupg_catalog::SequenceData> {
        self.env.sequence_row(file)
    }

    fn deparse(&self, text: &str, relation: u32) -> Result<String> {
        self.env.deparse(text, relation)
    }

    fn temp_number(&self) -> Option<i32> {
        self.env.temp_number()
    }

    fn check_function(&self, function: &rupg_catalog::Function) -> Result<()> {
        self.env.check_function(function)
    }

    fn function_setting(
        &self,
        set: &rupg_sql::nodes::VariableSetStmt,
    ) -> Result<(String, Option<String>)> {
        self.env.function_setting(set)
    }

    fn push_function_settings(&self, config: &[String]) -> Result<()> {
        self.env.push_function_settings(config)
    }

    fn pop_function_settings(&self) {
        self.env.pop_function_settings();
    }

    fn command_counter_increment(&self, catalog: &Catalog) {
        let session = self.env.catalog();
        *self.relations.borrow_mut() = catalog
            .relations()
            .filter(|r| session.is_none_or(|s| s.relation(r.oid).is_none()))
            .map(|r| (r.namespace, r.name.clone(), r.oid))
            .collect();
        self.env.command_counter_increment(catalog);
    }

    fn evaluate(&self, expr: &Expr) -> Result<Value> {
        self.env.evaluate(expr)
    }

    fn compare(&self, ty: u32, a: &Value, b: &Value) -> Result<std::cmp::Ordering> {
        self.env.compare(ty, a, b)
    }

    fn deparse_constant(&self, text: &str) -> Result<String> {
        self.env.deparse_constant(text)
    }
}

/// A relation that a name gives.
#[derive(Clone, Copy, Debug)]
enum Found {
    /// A relation of the catalog, with its OID.
    User(u32),
    /// A relation of the built-in catalog.
    Builtin(&'static ClassRow),
}

/// The state of one statement that defines an object.
pub(crate) struct Definer<'a, 'c> {
    an: Analyzer<'a>,
    catalog: &'c mut Catalog,
    user: u32,
    messages: Vec<Message>,
    /// The tables of [`Defined::scans`].
    scans: Vec<u32>,
    /// The tables of [`Defined::truncated`].
    truncated: Vec<u32>,
    /// The expressions of [`Defined::transforms`].
    transforms: Vec<Transform>,
    /// The defaults of the new columns, from which [`Defined::fills`] keeps the changed ones.
    added: Vec<Fill>,
    /// The views of [`Defined::views`].
    views: Vec<u32>,
    /// The partitions of [`Defined::checks`].
    checks: Vec<PartitionCheck>,
    /// The partitions of [`Defined::detached`].
    detached: Vec<u32>,
    /// The roles of [`Defined::grantors`].
    grantors: Vec<u32>,
    /// The elements of [`Defined::schema`].
    schema: Option<SchemaElements>,
    /// The check of [`Defined::top_level`].
    top_level: Option<TopLevel>,
    /// The index of [`Defined::concurrent`].
    concurrent: Option<u32>,
}

/// An error for a part of a statement that rupg does not support yet.
fn not_yet(what: &str, at: Option<usize>) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet")).at_opt(at)
}

/// The name of a relation as the query wrote it, with the schema and the database.
fn qualified(rv: &RangeVar) -> String {
    [rv.catalogname.as_deref(), rv.schemaname.as_deref(), rv.relname.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(".")
}

/// The name of the relation of a `RangeVar`.
fn relname(rv: &RangeVar) -> &str {
    rv.relname.as_deref().unwrap_or_default()
}

/// `contain_mutable_functions_after_planning`: true when the expression calls a function that is not immutable. The planner inlines `textanycat` and `anytextcat`, so a call of them is mutable only when the output function of the type of the other argument is.
fn is_mutable(catalog: &Catalog, expr: &Expr) -> bool {
    let mutable_proc = |oid: u32| match builtin::proc_by_oid(oid) {
        Some(proc) => proc.volatile != b'i',
        None => catalog.function(oid).is_none_or(|f| f.volatile != b'i'),
    };
    expr.find(0, &mut |e, _| {
        let mutable = match &e.kind {
            // `textanycat` and `anytextcat`: `$1 || $2::text` and `$1::text || $2`.
            ExprKind::Func(f) if matches!(f.oid, 2003 | 2004) => {
                let other = f.args.get(if f.oid == 2003 { 1 } else { 0 });
                other.is_none_or(|arg| {
                    mutable_proc(builtin::type_by_oid(arg.ty).map_or(0, |t| t.output))
                })
            }
            ExprKind::Func(f) => mutable_proc(f.oid),
            ExprKind::NullIf { equal, .. } | ExprKind::Distinct { equal, .. } => {
                mutable_proc(*equal)
            }
            ExprKind::ScalarArrayOp { func, .. } => mutable_proc(*func),
            ExprKind::CoerceViaIo(arg, _) => {
                let output = builtin::type_by_oid(arg.ty).map_or(0, |t| t.output);
                let input = builtin::type_by_oid(e.ty).map_or(0, |t| t.input);
                mutable_proc(output) || mutable_proc(input)
            }
            ExprKind::SqlValue(_) => true,
            _ => false,
        };
        mutable.then_some(())
    })
    .is_some()
}

/// The attribute numbers of the columns of the relation that the expressions read, in the order of their first use, without duplicates.
fn column_numbers(exprs: &[&Expr]) -> Vec<i16> {
    let mut numbers = Vec::new();
    for expr in exprs {
        expr.find(0, &mut |e, depth| {
            if let ExprKind::Var(var) = e.kind
                && var.levels_up == depth
                && !numbers.contains(&var.attnum)
            {
                numbers.push(var.attnum);
            }
            None::<()>
        });
    }
    numbers
}

/// `recordDependencyOnSingleRelExpr`: the objects that the expressions refer to. These are the columns of the relation, the functions that the expressions call, the relations and types of `regclass` and `regtype` constants, and the fields of composite types that the expressions select. The catalog drops the references to pinned objects. A `regrole` or `regdatabase` constant is an error, because a role and a database are shared objects.
fn references(catalog: &Catalog, relation: u32, exprs: &[&Expr]) -> Result<Vec<ObjRef>> {
    shared_constants(exprs)?;
    let mut refs: Vec<ObjRef> =
        column_numbers(exprs).into_iter().map(|n| ObjRef::column(relation, n)).collect();
    for expr in exprs {
        expr.find(0, &mut |e, _| {
            let found = match (&e.kind, e.ty) {
                (ExprKind::Const(Value::Oid(oid)), oid::REGCLASS) => {
                    Some(ObjRef::new(PG_CLASS, *oid))
                }
                (ExprKind::Const(Value::Oid(oid)), oid::REGTYPE) => {
                    Some(ObjRef::new(PG_TYPE, *oid))
                }
                (ExprKind::Func(f), _) => Some(ObjRef::new(PG_PROC, f.oid)),
                _ => field_reference(catalog, e),
            };
            for found in found.into_iter().chain(type_reference(e)) {
                if !refs.contains(&found) {
                    refs.push(found);
                }
            }
            None::<()>
        });
    }
    Ok(refs)
}

/// The error of `find_expr_references_walker` for a `regrole` or `regdatabase` constant in the expressions, because a role and a database are shared objects.
fn shared_constants(exprs: &[&Expr]) -> Result<()> {
    for expr in exprs {
        let shared = expr.find(0, &mut |e, _| match (&e.kind, e.ty) {
            (ExprKind::Const(Value::Oid(_)), oid::REGROLE) => Some("regrole"),
            (ExprKind::Const(Value::Oid(_)), oid::REGDATABASE) => Some("regdatabase"),
            _ => None,
        });
        if let Some(name) = shared {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("constant of the type {name} cannot be used here"),
            ));
        }
    }
    Ok(())
}

/// The column that `find_expr_references_walker` records for a `FieldSelect` of a composite type of the catalog, also through a domain: the column of the relation of the type that has the field.
pub(crate) fn field_reference(catalog: &Catalog, expr: &Expr) -> Option<ObjRef> {
    let ExprKind::FieldSelect(arg, field) = &expr.kind else { return None };
    let rel = row_type::row_relation(catalog, arg.ty)?;
    let (at, _) = rel.columns.iter().enumerate().filter(|(_, c)| !c.dropped).nth(*field)?;
    Some(ObjRef::column(rel.oid, i16::try_from(at + 1).ok()?))
}

/// The type that `find_expr_references_walker` records for a constant, a cast or the `Param` of a column of a subquery, which is the type of its result. The catalog drops the references to the built-in types, so only a type such as a domain of `initdb` stays.
pub(crate) fn type_reference(expr: &Expr) -> Option<ObjRef> {
    match expr.kind {
        ExprKind::Const(_)
        | ExprKind::Relabel(..)
        | ExprKind::CoerceViaIo(..)
        | ExprKind::ArrayCoerce { .. }
        | ExprKind::CoerceToDomain(..)
        | ExprKind::SubColumn(_) => Some(ObjRef::new(PG_TYPE, expr.ty)),
        _ => None,
    }
}

impl Definer<'_, '_> {
    /// Moves the warnings and the notices of the analyzer to the messages.
    fn flush(&mut self) {
        self.messages.extend(self.an.notices.drain(..).map(|error| {
            if error.is_notice() { Message::Notice(error) } else { Message::Warning(error) }
        }));
    }

    /// Adds a `NOTICE` after the warnings that came before it.
    fn notice(&mut self, state: SqlState, message: String) {
        self.flush();
        self.messages.push(Message::Notice(Error::new(state, message)));
    }

    /// Adds a `WARNING` after the messages that came before it.
    fn warning(&mut self, state: SqlState, message: String) {
        self.flush();
        self.messages.push(Message::Warning(Error::new(state, message)));
    }

    /// True when the role of the statement is a superuser.
    fn is_superuser(&self) -> bool {
        rupg_pgcatalog::roles::current().superuser(self.user)
    }

    /// The built-in schema with the name, unless `DROP SCHEMA` removed it.
    fn builtin_schema(&self, name: &str) -> Option<&'static NamedRow> {
        builtin::named(Named::Namespace)
            .iter()
            .find(|n| n.name == name && !self.catalog.schema_dropped(n.oid))
    }

    /// The owner of a schema, or `None` for a schema that does not exist.
    fn schema_owner(&self, namespace: u32) -> Option<u32> {
        match self.catalog.schema(namespace) {
            Some(schema) => Some(schema.owner),
            None if self.catalog.schema_dropped(namespace) => None,
            None => builtin::owned_by_oid(Owned::Namespace, namespace).map(|n| n.owner),
        }
    }

    /// The name of a schema.
    fn schema_name(&self, namespace: u32) -> String {
        match self.catalog.schema(namespace) {
            Some(schema) => schema.name.clone(),
            None => builtin::named_by_oid(Named::Namespace, namespace)
                .filter(|n| !self.catalog.schema_dropped(n.oid))
                .map_or_else(String::new, |n| n.name.to_string()),
        }
    }

    /// `object_aclcheck` of `CREATE` on a schema for a role: a superuser, the owner of the schema and a role that `nspacl` grants `CREATE` can create objects in it.
    fn may_create_in(&self, namespace: u32, role: u32) -> bool {
        rupg_pgcatalog::roles::current().superuser(role)
            || self.schema_owner(namespace) == Some(role)
            || self.schema_create_allowed(namespace, role)
    }

    /// `LookupCreationNamespace` for `SET SCHEMA`: the schema of the name, after the check of `CREATE` on it. `pg_temp` is the temporary schema of the session, which this makes when it does not exist.
    fn lookup_creation_namespace(&mut self, schema: &str) -> Result<u32> {
        let namespace = if schema == "pg_temp" {
            self.temp_namespace()?
        } else {
            self.an.schema(schema).ok_or_else(|| {
                Error::new(
                    SqlState::UNDEFINED_SCHEMA,
                    format!("schema \"{schema}\" does not exist"),
                )
            })?
        };
        self.check_create_in(namespace, None)?;
        Ok(namespace)
    }

    /// `CheckSetNamespace`: an object cannot move into or out of a temporary schema or the TOAST schema.
    fn check_set_namespace(&self, old: u32, new: u32) -> Result<()> {
        if self.catalog.is_temp_schema(old) || self.catalog.is_temp_schema(new) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot move objects into or out of temporary schemas",
            ));
        }
        if old == PG_TOAST_NAMESPACE || new == PG_TOAST_NAMESPACE {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot move objects into or out of TOAST schema",
            ));
        }
        Ok(())
    }

    /// The check of `CREATE` on a schema for the role of the statement.
    fn check_create_in(&self, namespace: u32, at: Option<usize>) -> Result<()> {
        if self.may_create_in(namespace, self.user) {
            return Ok(());
        }
        Err(Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("permission denied for schema {}", self.schema_name(namespace)),
        )
        .at_opt(at))
    }

    /// `RangeVarGetAndCheckCreationNamespace`: the schema where a new relation goes. A temporary relation and a relation in `pg_temp` go to the temporary schema of the session, and `RangeVarAdjustRelationPersistence` checks the persistence of the relation against its schema.
    fn creation_namespace(&mut self, rv: &RangeVar) -> Result<u32> {
        let at = place(rv.location);
        if let Some(catalog) = rv.catalogname.as_deref()
            && catalog != self.an.env.database()
        {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cross-database references are not implemented: \"{}\"", qualified(rv)),
            )
            .at_opt(at));
        }
        let unlogged = rv.relpersistence == b'u';
        // `RangeVarAdjustRelationPersistence`: an unlogged relation cannot go to a temporary schema.
        let only_temporary = || {
            Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "only temporary relations may be created in temporary schemas",
            )
            .at_opt(at)
        };
        let temp = rv.relpersistence == b't';
        if rv.schemaname.as_deref() == Some("pg_temp") || (temp && rv.schemaname.is_none()) {
            let namespace = self.temp_namespace()?;
            return if unlogged { Err(only_temporary()) } else { Ok(namespace) };
        }
        let namespace = match rv.schemaname.as_deref() {
            Some(schema) => self.an.schema(schema).ok_or_else(|| {
                Error::new(
                    SqlState::UNDEFINED_SCHEMA,
                    format!("schema \"{schema}\" does not exist"),
                )
                .at_opt(at)
            })?,
            None => self.an.env.search_path().first().copied().ok_or_else(|| {
                Error::new(SqlState::UNDEFINED_SCHEMA, "no schema has been selected to create in")
                    .at_opt(at)
            })?,
        };
        if unlogged && self.catalog.is_temp_schema(namespace) {
            return Err(only_temporary());
        }
        let own = namespace == self.an.temp || self.is_own_toast_temp(namespace);
        if !own && self.catalog.is_temp_schema(namespace) {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "cannot create relations in temporary schemas of other sessions",
            ));
        }
        if temp && !own {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "cannot create temporary relation in non-temporary schema",
            )
            .at_opt(at));
        }
        if !own {
            self.check_create_in(namespace, at)?;
        }
        Ok(namespace)
    }

    /// True for the TOAST schema `pg_toast_temp_N` of the temporary schema of the session.
    fn is_own_toast_temp(&self, namespace: u32) -> bool {
        let Some(n) = self.an.env.temp_number() else { return false };
        self.catalog.schema(namespace).is_some_and(|s| s.name == format!("pg_toast_temp_{n}"))
    }

    /// `AccessTempTableNamespace` and `InitTempTableNamespace`: the temporary schema `pg_temp_N` of the session. The first temporary object of the session makes it and its TOAST schema `pg_toast_temp_N`, which the bootstrap superuser owns.
    fn temp_namespace(&mut self) -> Result<u32> {
        if self.an.temp != 0 {
            return Ok(self.an.temp);
        }
        let Some(n) = self.an.env.temp_number() else {
            return Err(not_yet("a temporary object in a session with no server", None));
        };
        let name = format!("pg_temp_{n}");
        let oid = match self.catalog.schema_by_name(&name) {
            Some(schema) => schema.oid,
            None => {
                let oid = self.catalog.create_schema(&name, BOOTSTRAP_SUPERUSER)?;
                self.catalog.create_schema(&format!("pg_toast_temp_{n}"), BOOTSTRAP_SUPERUSER)?;
                oid
            }
        };
        self.an.temp = oid;
        self.an.path.retain(|&ns| ns != oid);
        if !self.an.temp_path.contains(&oid) {
            self.an.temp_path.insert(0, oid);
        }
        Ok(oid)
    }

    /// A relation with this name in the schema, of the catalog or built in.
    fn relation_in(&self, namespace: u32, name: &str) -> Option<Found> {
        if let Some(rel) = self.catalog.relation_by_name(namespace, name) {
            return Some(Found::User(rel.oid));
        }
        builtin::named(Named::Class)
            .iter()
            .find(|r| r.namespace == namespace && r.name == name)
            .and_then(|r| builtin::class_by_oid(r.oid))
            .map(Found::Builtin)
    }

    /// `RangeVarGetRelid`: the relation that a name gives. The errors have no position, as in PostgreSQL.
    fn lookup_relation(&self, rv: &RangeVar) -> Result<Found> {
        let name = relname(rv);
        if let Some(catalog) = rv.catalogname.as_deref()
            && catalog != self.an.env.database()
        {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cross-database references are not implemented: \"{}\"", qualified(rv)),
            ));
        }
        let found = match rv.schemaname.as_deref() {
            Some(schema) => {
                let namespace = self.an.explicit_schema(schema, None)?.ok_or_else(|| {
                    Error::new(
                        SqlState::UNDEFINED_SCHEMA,
                        format!("schema \"{schema}\" does not exist"),
                    )
                })?;
                self.relation_in(namespace, name)
            }
            None => self.an.temp_path.iter().find_map(|&ns| self.relation_in(ns, name)),
        };
        found.ok_or_else(|| {
            let shown = match rv.schemaname.as_deref() {
                Some(schema) => format!("{schema}.{name}"),
                None => name.to_string(),
            };
            Error::new(SqlState::UNDEFINED_TABLE, format!("relation \"{shown}\" does not exist"))
        })
    }

    /// `IsSystemRelation`: true for a relation of the catalog and for a TOAST table or its index.
    fn is_system(&self, found: Found) -> bool {
        match found {
            Found::Builtin(row) => row.oid < rupg_catalog::FIRST_UNPINNED_OID,
            Found::User(oid) => {
                self.catalog.relation(oid).is_some_and(|r| r.namespace == PG_TOAST_NAMESPACE)
            }
        }
    }

    /// The kind of a relation as a `relkind` code.
    fn kind_of(&self, found: Found) -> u8 {
        match found {
            Found::User(oid) => match self.catalog.relation(oid).map(|r| r.kind) {
                Some(RelKind::Index) => b'i',
                Some(RelKind::Sequence) => b'S',
                Some(RelKind::Toast) => b't',
                Some(RelKind::View) => b'v',
                Some(RelKind::Composite) => b'c',
                Some(RelKind::Partitioned) => b'p',
                Some(RelKind::PartitionedIndex) => b'I',
                Some(RelKind::MatView) => b'm',
                Some(RelKind::Foreign) => b'f',
                _ => b'r',
            },
            Found::Builtin(row) => row.kind,
        }
    }

    /// `CreateSchemaCommand`. The elements of the statement go to [`Defined::schema`] with their parts of `text`, and the caller runs them.
    fn create_schema(&mut self, stmt: &CreateSchemaStmt, text: &str) -> Result<()> {
        let owner = match &stmt.authrole {
            None => self.user,
            Some(role) => self.role_oid(role)?,
        };
        let roles = rupg_pgcatalog::roles::current();
        let owner_name = roles.name(owner);
        let name = match stmt.schemaname.as_deref() {
            Some(name) => name.to_string(),
            None => owner_name.unwrap_or_default().to_string(),
        };
        if !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for database {}", self.an.env.database()),
            ));
        }
        if name.starts_with("pg_") {
            return Err(Error::new(
                SqlState::RESERVED_NAME,
                format!("unacceptable schema name \"{name}\""),
            )
            .with_detail("The prefix \"pg_\" is reserved for system schemas."));
        }
        let exists =
            self.catalog.schema_by_name(&name).is_some() || self.builtin_schema(&name).is_some();
        if exists && stmt.if_not_exists {
            self.notice(
                SqlState::DUPLICATE_SCHEMA,
                format!("schema \"{name}\" already exists, skipping"),
            );
            return Ok(());
        }
        if exists {
            return Err(Error::new(
                SqlState::DUPLICATE_SCHEMA,
                format!("schema \"{name}\" already exists"),
            ));
        }
        let schema = self.catalog.create_schema(&name, owner)?;
        self.apply_default_acl(ObjRef::new(PG_NAMESPACE, schema))?;
        if !stmt.schemaElts.is_empty() {
            let elements: Vec<Node> = stmt.schemaElts.iter().flatten().cloned().collect();
            let elements = schema_elements::schema_elements(&elements, &name, text)?;
            self.schema = Some(SchemaElements { owner, schema: name, elements });
        }
        Ok(())
    }
}

/// The detail of `errdetail_relkind_not_supported`.
fn relkind_detail(kind: u8) -> &'static str {
    match kind {
        b'i' => "This operation is not supported for indexes.",
        b'I' => "This operation is not supported for partitioned indexes.",
        b'p' => "This operation is not supported for partitioned tables.",
        b'S' => "This operation is not supported for sequences.",
        b'v' => "This operation is not supported for views.",
        b'm' => "This operation is not supported for materialized views.",
        b'c' => "This operation is not supported for composite types.",
        b'f' => "This operation is not supported for foreign tables.",
        b't' => "This operation is not supported for TOAST tables.",
        _ => "This operation is not supported for tables.",
    }
}

/// The OID of the schema `pg_catalog` or `pg_toast`, where only the bootstrap can create relations.
fn is_system_namespace(namespace: u32) -> bool {
    namespace == PG_CATALOG_NAMESPACE || namespace == PG_TOAST_NAMESPACE
}
