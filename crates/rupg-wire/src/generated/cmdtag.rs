//! The command tags of PostgreSQL, one for each line of `cmdtaglist.h`.
//!
//! Made from `vendor/postgres-19/src/include/tcop/cmdtaglist.h`. Do not edit it by hand. The test `the_table_is_the_vendored_file` fails if this table and the vendored file disagree.
//!
//! Lifted from `crates/rudb-pgwire/src/generated/cmdtag.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

/// The tag of a statement, which `CommandComplete` sends. The variants are in the order of `cmdtaglist.h`, which is the order of the names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CommandTag {
    /// `???`.
    Unknown,
    /// `ALTER ACCESS METHOD`.
    AlterAccessMethod,
    /// `ALTER AGGREGATE`.
    AlterAggregate,
    /// `ALTER CAST`.
    AlterCast,
    /// `ALTER COLLATION`.
    AlterCollation,
    /// `ALTER CONSTRAINT`.
    AlterConstraint,
    /// `ALTER CONVERSION`.
    AlterConversion,
    /// `ALTER DATABASE`.
    AlterDatabase,
    /// `ALTER DEFAULT PRIVILEGES`.
    AlterDefaultPrivileges,
    /// `ALTER DOMAIN`.
    AlterDomain,
    /// `ALTER EVENT TRIGGER`.
    AlterEventTrigger,
    /// `ALTER EXTENSION`.
    AlterExtension,
    /// `ALTER FOREIGN DATA WRAPPER`.
    AlterForeignDataWrapper,
    /// `ALTER FOREIGN TABLE`.
    AlterForeignTable,
    /// `ALTER FUNCTION`.
    AlterFunction,
    /// `ALTER INDEX`.
    AlterIndex,
    /// `ALTER LANGUAGE`.
    AlterLanguage,
    /// `ALTER LARGE OBJECT`.
    AlterLargeObject,
    /// `ALTER MATERIALIZED VIEW`.
    AlterMaterializedView,
    /// `ALTER OPERATOR`.
    AlterOperator,
    /// `ALTER OPERATOR CLASS`.
    AlterOperatorClass,
    /// `ALTER OPERATOR FAMILY`.
    AlterOperatorFamily,
    /// `ALTER POLICY`.
    AlterPolicy,
    /// `ALTER PROCEDURE`.
    AlterProcedure,
    /// `ALTER PUBLICATION`.
    AlterPublication,
    /// `ALTER ROLE`.
    AlterRole,
    /// `ALTER ROUTINE`.
    AlterRoutine,
    /// `ALTER RULE`.
    AlterRule,
    /// `ALTER SCHEMA`.
    AlterSchema,
    /// `ALTER SEQUENCE`.
    AlterSequence,
    /// `ALTER SERVER`.
    AlterServer,
    /// `ALTER STATISTICS`.
    AlterStatistics,
    /// `ALTER SUBSCRIPTION`.
    AlterSubscription,
    /// `ALTER SYSTEM`.
    AlterSystem,
    /// `ALTER TABLE`.
    AlterTable,
    /// `ALTER TABLESPACE`.
    AlterTablespace,
    /// `ALTER TEXT SEARCH CONFIGURATION`.
    AlterTextSearchConfiguration,
    /// `ALTER TEXT SEARCH DICTIONARY`.
    AlterTextSearchDictionary,
    /// `ALTER TEXT SEARCH PARSER`.
    AlterTextSearchParser,
    /// `ALTER TEXT SEARCH TEMPLATE`.
    AlterTextSearchTemplate,
    /// `ALTER TRANSFORM`.
    AlterTransform,
    /// `ALTER TRIGGER`.
    AlterTrigger,
    /// `ALTER TYPE`.
    AlterType,
    /// `ALTER USER MAPPING`.
    AlterUserMapping,
    /// `ALTER VIEW`.
    AlterView,
    /// `ANALYZE`.
    Analyze,
    /// `BEGIN`.
    Begin,
    /// `CALL`.
    Call,
    /// `CHECKPOINT`.
    Checkpoint,
    /// `CLOSE`.
    Close,
    /// `CLOSE CURSOR`.
    CloseCursor,
    /// `CLOSE CURSOR ALL`.
    CloseCursorAll,
    /// `CLUSTER`.
    Cluster,
    /// `COMMENT`.
    Comment,
    /// `COMMIT`.
    Commit,
    /// `COMMIT PREPARED`.
    CommitPrepared,
    /// `COPY`.
    Copy,
    /// `COPY FROM`.
    CopyFrom,
    /// `CREATE ACCESS METHOD`.
    CreateAccessMethod,
    /// `CREATE AGGREGATE`.
    CreateAggregate,
    /// `CREATE CAST`.
    CreateCast,
    /// `CREATE COLLATION`.
    CreateCollation,
    /// `CREATE CONSTRAINT`.
    CreateConstraint,
    /// `CREATE CONVERSION`.
    CreateConversion,
    /// `CREATE DATABASE`.
    CreateDatabase,
    /// `CREATE DOMAIN`.
    CreateDomain,
    /// `CREATE EVENT TRIGGER`.
    CreateEventTrigger,
    /// `CREATE EXTENSION`.
    CreateExtension,
    /// `CREATE FOREIGN DATA WRAPPER`.
    CreateForeignDataWrapper,
    /// `CREATE FOREIGN TABLE`.
    CreateForeignTable,
    /// `CREATE FUNCTION`.
    CreateFunction,
    /// `CREATE INDEX`.
    CreateIndex,
    /// `CREATE LANGUAGE`.
    CreateLanguage,
    /// `CREATE MATERIALIZED VIEW`.
    CreateMaterializedView,
    /// `CREATE OPERATOR`.
    CreateOperator,
    /// `CREATE OPERATOR CLASS`.
    CreateOperatorClass,
    /// `CREATE OPERATOR FAMILY`.
    CreateOperatorFamily,
    /// `CREATE POLICY`.
    CreatePolicy,
    /// `CREATE PROCEDURE`.
    CreateProcedure,
    /// `CREATE PUBLICATION`.
    CreatePublication,
    /// `CREATE ROLE`.
    CreateRole,
    /// `CREATE ROUTINE`.
    CreateRoutine,
    /// `CREATE RULE`.
    CreateRule,
    /// `CREATE SCHEMA`.
    CreateSchema,
    /// `CREATE SEQUENCE`.
    CreateSequence,
    /// `CREATE SERVER`.
    CreateServer,
    /// `CREATE STATISTICS`.
    CreateStatistics,
    /// `CREATE SUBSCRIPTION`.
    CreateSubscription,
    /// `CREATE TABLE`.
    CreateTable,
    /// `CREATE TABLE AS`.
    CreateTableAs,
    /// `CREATE TABLESPACE`.
    CreateTablespace,
    /// `CREATE TEXT SEARCH CONFIGURATION`.
    CreateTextSearchConfiguration,
    /// `CREATE TEXT SEARCH DICTIONARY`.
    CreateTextSearchDictionary,
    /// `CREATE TEXT SEARCH PARSER`.
    CreateTextSearchParser,
    /// `CREATE TEXT SEARCH TEMPLATE`.
    CreateTextSearchTemplate,
    /// `CREATE TRANSFORM`.
    CreateTransform,
    /// `CREATE TRIGGER`.
    CreateTrigger,
    /// `CREATE TYPE`.
    CreateType,
    /// `CREATE USER MAPPING`.
    CreateUserMapping,
    /// `CREATE VIEW`.
    CreateView,
    /// `DEALLOCATE`.
    Deallocate,
    /// `DEALLOCATE ALL`.
    DeallocateAll,
    /// `DECLARE CURSOR`.
    DeclareCursor,
    /// `DELETE`.
    Delete,
    /// `DISCARD`.
    Discard,
    /// `DISCARD ALL`.
    DiscardAll,
    /// `DISCARD PLANS`.
    DiscardPlans,
    /// `DISCARD SEQUENCES`.
    DiscardSequences,
    /// `DISCARD TEMP`.
    DiscardTemp,
    /// `DO`.
    Do,
    /// `DROP ACCESS METHOD`.
    DropAccessMethod,
    /// `DROP AGGREGATE`.
    DropAggregate,
    /// `DROP CAST`.
    DropCast,
    /// `DROP COLLATION`.
    DropCollation,
    /// `DROP CONSTRAINT`.
    DropConstraint,
    /// `DROP CONVERSION`.
    DropConversion,
    /// `DROP DATABASE`.
    DropDatabase,
    /// `DROP DOMAIN`.
    DropDomain,
    /// `DROP EVENT TRIGGER`.
    DropEventTrigger,
    /// `DROP EXTENSION`.
    DropExtension,
    /// `DROP FOREIGN DATA WRAPPER`.
    DropForeignDataWrapper,
    /// `DROP FOREIGN TABLE`.
    DropForeignTable,
    /// `DROP FUNCTION`.
    DropFunction,
    /// `DROP INDEX`.
    DropIndex,
    /// `DROP LANGUAGE`.
    DropLanguage,
    /// `DROP MATERIALIZED VIEW`.
    DropMaterializedView,
    /// `DROP OPERATOR`.
    DropOperator,
    /// `DROP OPERATOR CLASS`.
    DropOperatorClass,
    /// `DROP OPERATOR FAMILY`.
    DropOperatorFamily,
    /// `DROP OWNED`.
    DropOwned,
    /// `DROP POLICY`.
    DropPolicy,
    /// `DROP PROCEDURE`.
    DropProcedure,
    /// `DROP PUBLICATION`.
    DropPublication,
    /// `DROP ROLE`.
    DropRole,
    /// `DROP ROUTINE`.
    DropRoutine,
    /// `DROP RULE`.
    DropRule,
    /// `DROP SCHEMA`.
    DropSchema,
    /// `DROP SEQUENCE`.
    DropSequence,
    /// `DROP SERVER`.
    DropServer,
    /// `DROP STATISTICS`.
    DropStatistics,
    /// `DROP SUBSCRIPTION`.
    DropSubscription,
    /// `DROP TABLE`.
    DropTable,
    /// `DROP TABLESPACE`.
    DropTablespace,
    /// `DROP TEXT SEARCH CONFIGURATION`.
    DropTextSearchConfiguration,
    /// `DROP TEXT SEARCH DICTIONARY`.
    DropTextSearchDictionary,
    /// `DROP TEXT SEARCH PARSER`.
    DropTextSearchParser,
    /// `DROP TEXT SEARCH TEMPLATE`.
    DropTextSearchTemplate,
    /// `DROP TRANSFORM`.
    DropTransform,
    /// `DROP TRIGGER`.
    DropTrigger,
    /// `DROP TYPE`.
    DropType,
    /// `DROP USER MAPPING`.
    DropUserMapping,
    /// `DROP VIEW`.
    DropView,
    /// `EXECUTE`.
    Execute,
    /// `EXPLAIN`.
    Explain,
    /// `FETCH`.
    Fetch,
    /// `GRANT`.
    Grant,
    /// `GRANT ROLE`.
    GrantRole,
    /// `IMPORT FOREIGN SCHEMA`.
    ImportForeignSchema,
    /// `INSERT`.
    Insert,
    /// `LISTEN`.
    Listen,
    /// `LOAD`.
    Load,
    /// `LOCK TABLE`.
    LockTable,
    /// `LOGIN`.
    Login,
    /// `MERGE`.
    Merge,
    /// `MOVE`.
    Move,
    /// `NOTIFY`.
    Notify,
    /// `PREPARE`.
    Prepare,
    /// `PREPARE TRANSACTION`.
    PrepareTransaction,
    /// `REASSIGN OWNED`.
    ReassignOwned,
    /// `REFRESH MATERIALIZED VIEW`.
    RefreshMaterializedView,
    /// `REINDEX`.
    Reindex,
    /// `RELEASE`.
    Release,
    /// `REPACK`.
    Repack,
    /// `RESET`.
    Reset,
    /// `REVOKE`.
    Revoke,
    /// `REVOKE ROLE`.
    RevokeRole,
    /// `ROLLBACK`.
    Rollback,
    /// `ROLLBACK PREPARED`.
    RollbackPrepared,
    /// `SAVEPOINT`.
    Savepoint,
    /// `SECURITY LABEL`.
    SecurityLabel,
    /// `SELECT`.
    Select,
    /// `SELECT FOR KEY SHARE`.
    SelectForKeyShare,
    /// `SELECT FOR NO KEY UPDATE`.
    SelectForNoKeyUpdate,
    /// `SELECT FOR SHARE`.
    SelectForShare,
    /// `SELECT FOR UPDATE`.
    SelectForUpdate,
    /// `SELECT INTO`.
    SelectInto,
    /// `SET`.
    Set,
    /// `SET CONSTRAINTS`.
    SetConstraints,
    /// `SHOW`.
    Show,
    /// `START TRANSACTION`.
    StartTransaction,
    /// `TRUNCATE TABLE`.
    TruncateTable,
    /// `UNLISTEN`.
    Unlisten,
    /// `UPDATE`.
    Update,
    /// `VACUUM`.
    Vacuum,
    /// `WAIT`.
    Wait,
}

/// Every tag in the order of the enum: the tag, the name, and the flags `event_trigger_ok`, `table_rewrite_ok` and `rowcount`.
pub(crate) static TAGS: [(CommandTag, &str, bool, bool, bool); 195] = [
    (CommandTag::Unknown, "???", false, false, false),
    (CommandTag::AlterAccessMethod, "ALTER ACCESS METHOD", true, false, false),
    (CommandTag::AlterAggregate, "ALTER AGGREGATE", true, false, false),
    (CommandTag::AlterCast, "ALTER CAST", true, false, false),
    (CommandTag::AlterCollation, "ALTER COLLATION", true, false, false),
    (CommandTag::AlterConstraint, "ALTER CONSTRAINT", true, false, false),
    (CommandTag::AlterConversion, "ALTER CONVERSION", true, false, false),
    (CommandTag::AlterDatabase, "ALTER DATABASE", false, false, false),
    (CommandTag::AlterDefaultPrivileges, "ALTER DEFAULT PRIVILEGES", true, false, false),
    (CommandTag::AlterDomain, "ALTER DOMAIN", true, false, false),
    (CommandTag::AlterEventTrigger, "ALTER EVENT TRIGGER", false, false, false),
    (CommandTag::AlterExtension, "ALTER EXTENSION", true, false, false),
    (CommandTag::AlterForeignDataWrapper, "ALTER FOREIGN DATA WRAPPER", true, false, false),
    (CommandTag::AlterForeignTable, "ALTER FOREIGN TABLE", true, false, false),
    (CommandTag::AlterFunction, "ALTER FUNCTION", true, false, false),
    (CommandTag::AlterIndex, "ALTER INDEX", true, false, false),
    (CommandTag::AlterLanguage, "ALTER LANGUAGE", true, false, false),
    (CommandTag::AlterLargeObject, "ALTER LARGE OBJECT", true, false, false),
    (CommandTag::AlterMaterializedView, "ALTER MATERIALIZED VIEW", true, true, false),
    (CommandTag::AlterOperator, "ALTER OPERATOR", true, false, false),
    (CommandTag::AlterOperatorClass, "ALTER OPERATOR CLASS", true, false, false),
    (CommandTag::AlterOperatorFamily, "ALTER OPERATOR FAMILY", true, false, false),
    (CommandTag::AlterPolicy, "ALTER POLICY", true, false, false),
    (CommandTag::AlterProcedure, "ALTER PROCEDURE", true, false, false),
    (CommandTag::AlterPublication, "ALTER PUBLICATION", true, false, false),
    (CommandTag::AlterRole, "ALTER ROLE", false, false, false),
    (CommandTag::AlterRoutine, "ALTER ROUTINE", true, false, false),
    (CommandTag::AlterRule, "ALTER RULE", true, false, false),
    (CommandTag::AlterSchema, "ALTER SCHEMA", true, false, false),
    (CommandTag::AlterSequence, "ALTER SEQUENCE", true, false, false),
    (CommandTag::AlterServer, "ALTER SERVER", true, false, false),
    (CommandTag::AlterStatistics, "ALTER STATISTICS", true, false, false),
    (CommandTag::AlterSubscription, "ALTER SUBSCRIPTION", true, false, false),
    (CommandTag::AlterSystem, "ALTER SYSTEM", false, false, false),
    (CommandTag::AlterTable, "ALTER TABLE", true, true, false),
    (CommandTag::AlterTablespace, "ALTER TABLESPACE", false, false, false),
    (
        CommandTag::AlterTextSearchConfiguration,
        "ALTER TEXT SEARCH CONFIGURATION",
        true,
        false,
        false,
    ),
    (CommandTag::AlterTextSearchDictionary, "ALTER TEXT SEARCH DICTIONARY", true, false, false),
    (CommandTag::AlterTextSearchParser, "ALTER TEXT SEARCH PARSER", true, false, false),
    (CommandTag::AlterTextSearchTemplate, "ALTER TEXT SEARCH TEMPLATE", true, false, false),
    (CommandTag::AlterTransform, "ALTER TRANSFORM", true, false, false),
    (CommandTag::AlterTrigger, "ALTER TRIGGER", true, false, false),
    (CommandTag::AlterType, "ALTER TYPE", true, true, false),
    (CommandTag::AlterUserMapping, "ALTER USER MAPPING", true, false, false),
    (CommandTag::AlterView, "ALTER VIEW", true, false, false),
    (CommandTag::Analyze, "ANALYZE", false, false, false),
    (CommandTag::Begin, "BEGIN", false, false, false),
    (CommandTag::Call, "CALL", false, false, false),
    (CommandTag::Checkpoint, "CHECKPOINT", false, false, false),
    (CommandTag::Close, "CLOSE", false, false, false),
    (CommandTag::CloseCursor, "CLOSE CURSOR", false, false, false),
    (CommandTag::CloseCursorAll, "CLOSE CURSOR ALL", false, false, false),
    (CommandTag::Cluster, "CLUSTER", false, false, false),
    (CommandTag::Comment, "COMMENT", true, false, false),
    (CommandTag::Commit, "COMMIT", false, false, false),
    (CommandTag::CommitPrepared, "COMMIT PREPARED", false, false, false),
    (CommandTag::Copy, "COPY", false, false, true),
    (CommandTag::CopyFrom, "COPY FROM", false, false, false),
    (CommandTag::CreateAccessMethod, "CREATE ACCESS METHOD", true, false, false),
    (CommandTag::CreateAggregate, "CREATE AGGREGATE", true, false, false),
    (CommandTag::CreateCast, "CREATE CAST", true, false, false),
    (CommandTag::CreateCollation, "CREATE COLLATION", true, false, false),
    (CommandTag::CreateConstraint, "CREATE CONSTRAINT", true, false, false),
    (CommandTag::CreateConversion, "CREATE CONVERSION", true, false, false),
    (CommandTag::CreateDatabase, "CREATE DATABASE", false, false, false),
    (CommandTag::CreateDomain, "CREATE DOMAIN", true, false, false),
    (CommandTag::CreateEventTrigger, "CREATE EVENT TRIGGER", false, false, false),
    (CommandTag::CreateExtension, "CREATE EXTENSION", true, false, false),
    (CommandTag::CreateForeignDataWrapper, "CREATE FOREIGN DATA WRAPPER", true, false, false),
    (CommandTag::CreateForeignTable, "CREATE FOREIGN TABLE", true, false, false),
    (CommandTag::CreateFunction, "CREATE FUNCTION", true, false, false),
    (CommandTag::CreateIndex, "CREATE INDEX", true, false, false),
    (CommandTag::CreateLanguage, "CREATE LANGUAGE", true, false, false),
    (CommandTag::CreateMaterializedView, "CREATE MATERIALIZED VIEW", true, false, false),
    (CommandTag::CreateOperator, "CREATE OPERATOR", true, false, false),
    (CommandTag::CreateOperatorClass, "CREATE OPERATOR CLASS", true, false, false),
    (CommandTag::CreateOperatorFamily, "CREATE OPERATOR FAMILY", true, false, false),
    (CommandTag::CreatePolicy, "CREATE POLICY", true, false, false),
    (CommandTag::CreateProcedure, "CREATE PROCEDURE", true, false, false),
    (CommandTag::CreatePublication, "CREATE PUBLICATION", true, false, false),
    (CommandTag::CreateRole, "CREATE ROLE", false, false, false),
    (CommandTag::CreateRoutine, "CREATE ROUTINE", true, false, false),
    (CommandTag::CreateRule, "CREATE RULE", true, false, false),
    (CommandTag::CreateSchema, "CREATE SCHEMA", true, false, false),
    (CommandTag::CreateSequence, "CREATE SEQUENCE", true, false, false),
    (CommandTag::CreateServer, "CREATE SERVER", true, false, false),
    (CommandTag::CreateStatistics, "CREATE STATISTICS", true, false, false),
    (CommandTag::CreateSubscription, "CREATE SUBSCRIPTION", true, false, false),
    (CommandTag::CreateTable, "CREATE TABLE", true, false, false),
    (CommandTag::CreateTableAs, "CREATE TABLE AS", true, false, false),
    (CommandTag::CreateTablespace, "CREATE TABLESPACE", false, false, false),
    (
        CommandTag::CreateTextSearchConfiguration,
        "CREATE TEXT SEARCH CONFIGURATION",
        true,
        false,
        false,
    ),
    (CommandTag::CreateTextSearchDictionary, "CREATE TEXT SEARCH DICTIONARY", true, false, false),
    (CommandTag::CreateTextSearchParser, "CREATE TEXT SEARCH PARSER", true, false, false),
    (CommandTag::CreateTextSearchTemplate, "CREATE TEXT SEARCH TEMPLATE", true, false, false),
    (CommandTag::CreateTransform, "CREATE TRANSFORM", true, false, false),
    (CommandTag::CreateTrigger, "CREATE TRIGGER", true, false, false),
    (CommandTag::CreateType, "CREATE TYPE", true, false, false),
    (CommandTag::CreateUserMapping, "CREATE USER MAPPING", true, false, false),
    (CommandTag::CreateView, "CREATE VIEW", true, false, false),
    (CommandTag::Deallocate, "DEALLOCATE", false, false, false),
    (CommandTag::DeallocateAll, "DEALLOCATE ALL", false, false, false),
    (CommandTag::DeclareCursor, "DECLARE CURSOR", false, false, false),
    (CommandTag::Delete, "DELETE", false, false, true),
    (CommandTag::Discard, "DISCARD", false, false, false),
    (CommandTag::DiscardAll, "DISCARD ALL", false, false, false),
    (CommandTag::DiscardPlans, "DISCARD PLANS", false, false, false),
    (CommandTag::DiscardSequences, "DISCARD SEQUENCES", false, false, false),
    (CommandTag::DiscardTemp, "DISCARD TEMP", false, false, false),
    (CommandTag::Do, "DO", false, false, false),
    (CommandTag::DropAccessMethod, "DROP ACCESS METHOD", true, false, false),
    (CommandTag::DropAggregate, "DROP AGGREGATE", true, false, false),
    (CommandTag::DropCast, "DROP CAST", true, false, false),
    (CommandTag::DropCollation, "DROP COLLATION", true, false, false),
    (CommandTag::DropConstraint, "DROP CONSTRAINT", true, false, false),
    (CommandTag::DropConversion, "DROP CONVERSION", true, false, false),
    (CommandTag::DropDatabase, "DROP DATABASE", false, false, false),
    (CommandTag::DropDomain, "DROP DOMAIN", true, false, false),
    (CommandTag::DropEventTrigger, "DROP EVENT TRIGGER", false, false, false),
    (CommandTag::DropExtension, "DROP EXTENSION", true, false, false),
    (CommandTag::DropForeignDataWrapper, "DROP FOREIGN DATA WRAPPER", true, false, false),
    (CommandTag::DropForeignTable, "DROP FOREIGN TABLE", true, false, false),
    (CommandTag::DropFunction, "DROP FUNCTION", true, false, false),
    (CommandTag::DropIndex, "DROP INDEX", true, false, false),
    (CommandTag::DropLanguage, "DROP LANGUAGE", true, false, false),
    (CommandTag::DropMaterializedView, "DROP MATERIALIZED VIEW", true, false, false),
    (CommandTag::DropOperator, "DROP OPERATOR", true, false, false),
    (CommandTag::DropOperatorClass, "DROP OPERATOR CLASS", true, false, false),
    (CommandTag::DropOperatorFamily, "DROP OPERATOR FAMILY", true, false, false),
    (CommandTag::DropOwned, "DROP OWNED", true, false, false),
    (CommandTag::DropPolicy, "DROP POLICY", true, false, false),
    (CommandTag::DropProcedure, "DROP PROCEDURE", true, false, false),
    (CommandTag::DropPublication, "DROP PUBLICATION", true, false, false),
    (CommandTag::DropRole, "DROP ROLE", false, false, false),
    (CommandTag::DropRoutine, "DROP ROUTINE", true, false, false),
    (CommandTag::DropRule, "DROP RULE", true, false, false),
    (CommandTag::DropSchema, "DROP SCHEMA", true, false, false),
    (CommandTag::DropSequence, "DROP SEQUENCE", true, false, false),
    (CommandTag::DropServer, "DROP SERVER", true, false, false),
    (CommandTag::DropStatistics, "DROP STATISTICS", true, false, false),
    (CommandTag::DropSubscription, "DROP SUBSCRIPTION", true, false, false),
    (CommandTag::DropTable, "DROP TABLE", true, false, false),
    (CommandTag::DropTablespace, "DROP TABLESPACE", false, false, false),
    (CommandTag::DropTextSearchConfiguration, "DROP TEXT SEARCH CONFIGURATION", true, false, false),
    (CommandTag::DropTextSearchDictionary, "DROP TEXT SEARCH DICTIONARY", true, false, false),
    (CommandTag::DropTextSearchParser, "DROP TEXT SEARCH PARSER", true, false, false),
    (CommandTag::DropTextSearchTemplate, "DROP TEXT SEARCH TEMPLATE", true, false, false),
    (CommandTag::DropTransform, "DROP TRANSFORM", true, false, false),
    (CommandTag::DropTrigger, "DROP TRIGGER", true, false, false),
    (CommandTag::DropType, "DROP TYPE", true, false, false),
    (CommandTag::DropUserMapping, "DROP USER MAPPING", true, false, false),
    (CommandTag::DropView, "DROP VIEW", true, false, false),
    (CommandTag::Execute, "EXECUTE", false, false, false),
    (CommandTag::Explain, "EXPLAIN", false, false, false),
    (CommandTag::Fetch, "FETCH", false, false, true),
    (CommandTag::Grant, "GRANT", true, false, false),
    (CommandTag::GrantRole, "GRANT ROLE", false, false, false),
    (CommandTag::ImportForeignSchema, "IMPORT FOREIGN SCHEMA", true, false, false),
    (CommandTag::Insert, "INSERT", false, false, true),
    (CommandTag::Listen, "LISTEN", false, false, false),
    (CommandTag::Load, "LOAD", false, false, false),
    (CommandTag::LockTable, "LOCK TABLE", false, false, false),
    (CommandTag::Login, "LOGIN", true, false, false),
    (CommandTag::Merge, "MERGE", false, false, true),
    (CommandTag::Move, "MOVE", false, false, true),
    (CommandTag::Notify, "NOTIFY", false, false, false),
    (CommandTag::Prepare, "PREPARE", false, false, false),
    (CommandTag::PrepareTransaction, "PREPARE TRANSACTION", false, false, false),
    (CommandTag::ReassignOwned, "REASSIGN OWNED", false, false, false),
    (CommandTag::RefreshMaterializedView, "REFRESH MATERIALIZED VIEW", true, false, false),
    (CommandTag::Reindex, "REINDEX", true, false, false),
    (CommandTag::Release, "RELEASE", false, false, false),
    (CommandTag::Repack, "REPACK", false, false, false),
    (CommandTag::Reset, "RESET", false, false, false),
    (CommandTag::Revoke, "REVOKE", true, false, false),
    (CommandTag::RevokeRole, "REVOKE ROLE", false, false, false),
    (CommandTag::Rollback, "ROLLBACK", false, false, false),
    (CommandTag::RollbackPrepared, "ROLLBACK PREPARED", false, false, false),
    (CommandTag::Savepoint, "SAVEPOINT", false, false, false),
    (CommandTag::SecurityLabel, "SECURITY LABEL", true, false, false),
    (CommandTag::Select, "SELECT", false, false, true),
    (CommandTag::SelectForKeyShare, "SELECT FOR KEY SHARE", false, false, false),
    (CommandTag::SelectForNoKeyUpdate, "SELECT FOR NO KEY UPDATE", false, false, false),
    (CommandTag::SelectForShare, "SELECT FOR SHARE", false, false, false),
    (CommandTag::SelectForUpdate, "SELECT FOR UPDATE", false, false, false),
    (CommandTag::SelectInto, "SELECT INTO", true, false, false),
    (CommandTag::Set, "SET", false, false, false),
    (CommandTag::SetConstraints, "SET CONSTRAINTS", false, false, false),
    (CommandTag::Show, "SHOW", false, false, false),
    (CommandTag::StartTransaction, "START TRANSACTION", false, false, false),
    (CommandTag::TruncateTable, "TRUNCATE TABLE", false, false, false),
    (CommandTag::Unlisten, "UNLISTEN", false, false, false),
    (CommandTag::Update, "UPDATE", false, false, true),
    (CommandTag::Vacuum, "VACUUM", false, false, false),
    (CommandTag::Wait, "WAIT", false, false, false),
];
