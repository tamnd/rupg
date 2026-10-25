//! `CreateCommandTag`: the name of the command tag of a statement, for the errors that name a statement.

use rupg_sql::nodes::{
    CreateTableAsStmt, DiscardMode, Node, ObjectType, TransactionStmtKind, VariableSetKind,
};

/// `CMDTAG_UNKNOWN`.
const UNKNOWN: &str = "???";

/// `CreateCommandTag`: the command tag of the statement `node` before its analysis.
pub(crate) fn command_tag(node: &Node) -> String {
    let fixed = match node {
        Node::InsertStmt(_) => "INSERT",
        Node::DeleteStmt(_) => "DELETE",
        Node::UpdateStmt(_) => "UPDATE",
        Node::MergeStmt(_) => "MERGE",
        Node::SelectStmt(_) | Node::PLAssignStmt(_) => "SELECT",
        Node::TransactionStmt(stmt) => transaction_tag(stmt.kind),
        Node::DeclareCursorStmt(_) => "DECLARE CURSOR",
        Node::ClosePortalStmt(stmt) if stmt.portalname.is_none() => "CLOSE CURSOR ALL",
        Node::ClosePortalStmt(_) => "CLOSE CURSOR",
        Node::FetchStmt(stmt) if stmt.ismove => "MOVE",
        Node::FetchStmt(_) => "FETCH",
        Node::CreateDomainStmt(_) => "CREATE DOMAIN",
        Node::CreateSchemaStmt(_) => "CREATE SCHEMA",
        Node::CreateStmt(_) => "CREATE TABLE",
        Node::CreateTableSpaceStmt(_) => "CREATE TABLESPACE",
        Node::DropTableSpaceStmt(_) => "DROP TABLESPACE",
        Node::AlterTableSpaceOptionsStmt(_) => "ALTER TABLESPACE",
        Node::CreateExtensionStmt(_) => "CREATE EXTENSION",
        Node::AlterExtensionStmt(_) | Node::AlterExtensionContentsStmt(_) => "ALTER EXTENSION",
        Node::CreateFdwStmt(_) => "CREATE FOREIGN DATA WRAPPER",
        Node::AlterFdwStmt(_) => "ALTER FOREIGN DATA WRAPPER",
        Node::CreateForeignServerStmt(_) => "CREATE SERVER",
        Node::AlterForeignServerStmt(_) => "ALTER SERVER",
        Node::CreateUserMappingStmt(_) => "CREATE USER MAPPING",
        Node::AlterUserMappingStmt(_) => "ALTER USER MAPPING",
        Node::DropUserMappingStmt(_) => "DROP USER MAPPING",
        Node::CreateForeignTableStmt(_) => "CREATE FOREIGN TABLE",
        Node::ImportForeignSchemaStmt(_) => "IMPORT FOREIGN SCHEMA",
        Node::DropStmt(stmt) => return drop_tag(stmt.removeType),
        Node::TruncateStmt(_) => "TRUNCATE TABLE",
        Node::CommentStmt(_) => "COMMENT",
        Node::SecLabelStmt(_) => "SECURITY LABEL",
        Node::CopyStmt(_) => "COPY",
        Node::RenameStmt(stmt) if stmt.renameType == ObjectType::OBJECT_COLUMN => {
            return alter_tag(stmt.relationType);
        }
        Node::RenameStmt(stmt) => return alter_tag(stmt.renameType),
        Node::AlterObjectDependsStmt(stmt) => return alter_tag(stmt.objectType),
        Node::AlterObjectSchemaStmt(stmt) => return alter_tag(stmt.objectType),
        Node::AlterOwnerStmt(stmt) => return alter_tag(stmt.objectType),
        Node::AlterTableMoveAllStmt(stmt) => return alter_tag(stmt.objtype),
        Node::AlterTableStmt(stmt) => return alter_tag(stmt.objtype),
        Node::AlterDomainStmt(_) => "ALTER DOMAIN",
        Node::AlterFunctionStmt(stmt) => match stmt.objtype {
            ObjectType::OBJECT_FUNCTION => "ALTER FUNCTION",
            ObjectType::OBJECT_PROCEDURE => "ALTER PROCEDURE",
            ObjectType::OBJECT_ROUTINE => "ALTER ROUTINE",
            _ => UNKNOWN,
        },
        Node::GrantStmt(stmt) if stmt.is_grant => "GRANT",
        Node::GrantStmt(_) => "REVOKE",
        Node::GrantRoleStmt(stmt) if stmt.is_grant => "GRANT ROLE",
        Node::GrantRoleStmt(_) => "REVOKE ROLE",
        Node::AlterDefaultPrivilegesStmt(_) => "ALTER DEFAULT PRIVILEGES",
        Node::DefineStmt(stmt) => match stmt.kind {
            ObjectType::OBJECT_AGGREGATE => "CREATE AGGREGATE",
            ObjectType::OBJECT_OPERATOR => "CREATE OPERATOR",
            ObjectType::OBJECT_TYPE => "CREATE TYPE",
            ObjectType::OBJECT_TSPARSER => "CREATE TEXT SEARCH PARSER",
            ObjectType::OBJECT_TSDICTIONARY => "CREATE TEXT SEARCH DICTIONARY",
            ObjectType::OBJECT_TSTEMPLATE => "CREATE TEXT SEARCH TEMPLATE",
            ObjectType::OBJECT_TSCONFIGURATION => "CREATE TEXT SEARCH CONFIGURATION",
            ObjectType::OBJECT_COLLATION => "CREATE COLLATION",
            ObjectType::OBJECT_ACCESS_METHOD => "CREATE ACCESS METHOD",
            _ => UNKNOWN,
        },
        Node::CompositeTypeStmt(_) | Node::CreateEnumStmt(_) | Node::CreateRangeStmt(_) => {
            "CREATE TYPE"
        }
        Node::AlterEnumStmt(_) | Node::AlterTypeStmt(_) => "ALTER TYPE",
        Node::ViewStmt(_) => "CREATE VIEW",
        Node::CreateFunctionStmt(stmt) if stmt.is_procedure => "CREATE PROCEDURE",
        Node::CreateFunctionStmt(_) => "CREATE FUNCTION",
        Node::IndexStmt(_) => "CREATE INDEX",
        Node::RuleStmt(_) => "CREATE RULE",
        Node::CreateSeqStmt(_) => "CREATE SEQUENCE",
        Node::AlterSeqStmt(_) => "ALTER SEQUENCE",
        Node::DoStmt(_) => "DO",
        Node::CreatedbStmt(_) => "CREATE DATABASE",
        Node::AlterDatabaseStmt(_)
        | Node::AlterDatabaseRefreshCollStmt(_)
        | Node::AlterDatabaseSetStmt(_) => "ALTER DATABASE",
        Node::DropdbStmt(_) => "DROP DATABASE",
        Node::NotifyStmt(_) => "NOTIFY",
        Node::ListenStmt(_) => "LISTEN",
        Node::UnlistenStmt(_) => "UNLISTEN",
        Node::LoadStmt(_) => "LOAD",
        Node::CallStmt(_) => "CALL",
        Node::VacuumStmt(stmt) if stmt.is_vacuumcmd => "VACUUM",
        Node::VacuumStmt(_) => "ANALYZE",
        Node::ExplainStmt(_) => "EXPLAIN",
        Node::CreateTableAsStmt(stmt) => table_as_tag(stmt),
        Node::RefreshMatViewStmt(_) => "REFRESH MATERIALIZED VIEW",
        Node::AlterSystemStmt(_) => "ALTER SYSTEM",
        Node::VariableSetStmt(stmt) => match stmt.kind {
            VariableSetKind::VAR_RESET | VariableSetKind::VAR_RESET_ALL => "RESET",
            _ => "SET",
        },
        Node::VariableShowStmt(_) => "SHOW",
        Node::DiscardStmt(stmt) => match stmt.target {
            DiscardMode::DISCARD_ALL => "DISCARD ALL",
            DiscardMode::DISCARD_PLANS => "DISCARD PLANS",
            DiscardMode::DISCARD_TEMP => "DISCARD TEMP",
            DiscardMode::DISCARD_SEQUENCES => "DISCARD SEQUENCES",
            _ => UNKNOWN,
        },
        Node::CreateTransformStmt(_) => "CREATE TRANSFORM",
        Node::CreateTrigStmt(_) => "CREATE TRIGGER",
        Node::CreateEventTrigStmt(_) => "CREATE EVENT TRIGGER",
        Node::AlterEventTrigStmt(_) => "ALTER EVENT TRIGGER",
        Node::CreatePLangStmt(_) => "CREATE LANGUAGE",
        Node::CreateRoleStmt(_) => "CREATE ROLE",
        Node::AlterRoleStmt(_) | Node::AlterRoleSetStmt(_) => "ALTER ROLE",
        Node::DropRoleStmt(_) => "DROP ROLE",
        Node::DropOwnedStmt(_) => "DROP OWNED",
        Node::ReassignOwnedStmt(_) => "REASSIGN OWNED",
        Node::LockStmt(_) => "LOCK TABLE",
        Node::ConstraintsSetStmt(_) => "SET CONSTRAINTS",
        Node::CheckPointStmt(_) => "CHECKPOINT",
        Node::ReindexStmt(_) => "REINDEX",
        Node::CreateConversionStmt(_) => "CREATE CONVERSION",
        Node::CreateCastStmt(_) => "CREATE CAST",
        Node::CreateOpClassStmt(_) => "CREATE OPERATOR CLASS",
        Node::CreateOpFamilyStmt(_) => "CREATE OPERATOR FAMILY",
        Node::AlterOpFamilyStmt(_) => "ALTER OPERATOR FAMILY",
        Node::AlterOperatorStmt(_) => "ALTER OPERATOR",
        Node::AlterTSDictionaryStmt(_) => "ALTER TEXT SEARCH DICTIONARY",
        Node::AlterTSConfigurationStmt(_) => "ALTER TEXT SEARCH CONFIGURATION",
        Node::CreatePolicyStmt(_) => "CREATE POLICY",
        Node::AlterPolicyStmt(_) => "ALTER POLICY",
        Node::CreateAmStmt(_) => "CREATE ACCESS METHOD",
        Node::CreatePublicationStmt(_) => "CREATE PUBLICATION",
        Node::AlterPublicationStmt(_) => "ALTER PUBLICATION",
        Node::CreateSubscriptionStmt(_) => "CREATE SUBSCRIPTION",
        Node::AlterSubscriptionStmt(_) => "ALTER SUBSCRIPTION",
        Node::DropSubscriptionStmt(_) => "DROP SUBSCRIPTION",
        Node::AlterCollationStmt(_) => "ALTER COLLATION",
        Node::PrepareStmt(_) => "PREPARE",
        Node::ExecuteStmt(_) => "EXECUTE",
        Node::CreateStatsStmt(_) => "CREATE STATISTICS",
        Node::AlterStatsStmt(_) => "ALTER STATISTICS",
        Node::DeallocateStmt(stmt) if stmt.name.is_none() => "DEALLOCATE ALL",
        Node::DeallocateStmt(_) => "DEALLOCATE",
        _ => UNKNOWN,
    };
    fixed.to_string()
}

/// The tag of a `TransactionStmt`.
fn transaction_tag(kind: TransactionStmtKind) -> &'static str {
    match kind {
        TransactionStmtKind::TRANS_STMT_BEGIN => "BEGIN",
        TransactionStmtKind::TRANS_STMT_START => "START TRANSACTION",
        TransactionStmtKind::TRANS_STMT_COMMIT => "COMMIT",
        TransactionStmtKind::TRANS_STMT_ROLLBACK | TransactionStmtKind::TRANS_STMT_ROLLBACK_TO => {
            "ROLLBACK"
        }
        TransactionStmtKind::TRANS_STMT_SAVEPOINT => "SAVEPOINT",
        TransactionStmtKind::TRANS_STMT_RELEASE => "RELEASE",
        TransactionStmtKind::TRANS_STMT_PREPARE => "PREPARE TRANSACTION",
        TransactionStmtKind::TRANS_STMT_COMMIT_PREPARED => "COMMIT PREPARED",
        TransactionStmtKind::TRANS_STMT_ROLLBACK_PREPARED => "ROLLBACK PREPARED",
        _ => UNKNOWN,
    }
}

/// The tag of a `CreateTableAsStmt`.
fn table_as_tag(stmt: &CreateTableAsStmt) -> &'static str {
    match stmt.objtype {
        ObjectType::OBJECT_TABLE if stmt.is_select_into => "SELECT INTO",
        ObjectType::OBJECT_TABLE => "CREATE TABLE AS",
        ObjectType::OBJECT_MATVIEW => "CREATE MATERIALIZED VIEW",
        _ => UNKNOWN,
    }
}

/// The name of a kind of object in the tags of `DROP` and `ALTER`.
fn object_word(kind: ObjectType) -> Option<&'static str> {
    Some(match kind {
        ObjectType::OBJECT_AGGREGATE => "AGGREGATE",
        ObjectType::OBJECT_CAST => "CAST",
        ObjectType::OBJECT_COLLATION => "COLLATION",
        ObjectType::OBJECT_CONVERSION => "CONVERSION",
        ObjectType::OBJECT_DATABASE => "DATABASE",
        ObjectType::OBJECT_DOMAIN => "DOMAIN",
        ObjectType::OBJECT_EXTENSION => "EXTENSION",
        ObjectType::OBJECT_FDW => "FOREIGN DATA WRAPPER",
        ObjectType::OBJECT_FOREIGN_SERVER => "SERVER",
        ObjectType::OBJECT_FOREIGN_TABLE => "FOREIGN TABLE",
        ObjectType::OBJECT_FUNCTION => "FUNCTION",
        ObjectType::OBJECT_INDEX => "INDEX",
        ObjectType::OBJECT_LANGUAGE => "LANGUAGE",
        ObjectType::OBJECT_LARGEOBJECT => "LARGE OBJECT",
        ObjectType::OBJECT_OPCLASS => "OPERATOR CLASS",
        ObjectType::OBJECT_OPERATOR => "OPERATOR",
        ObjectType::OBJECT_OPFAMILY => "OPERATOR FAMILY",
        ObjectType::OBJECT_POLICY => "POLICY",
        ObjectType::OBJECT_PROCEDURE => "PROCEDURE",
        ObjectType::OBJECT_ROLE => "ROLE",
        ObjectType::OBJECT_ROUTINE => "ROUTINE",
        ObjectType::OBJECT_RULE => "RULE",
        ObjectType::OBJECT_SCHEMA => "SCHEMA",
        ObjectType::OBJECT_SEQUENCE => "SEQUENCE",
        ObjectType::OBJECT_TABLE => "TABLE",
        ObjectType::OBJECT_TABLESPACE => "TABLESPACE",
        ObjectType::OBJECT_TRIGGER => "TRIGGER",
        ObjectType::OBJECT_EVENT_TRIGGER => "EVENT TRIGGER",
        ObjectType::OBJECT_TSCONFIGURATION => "TEXT SEARCH CONFIGURATION",
        ObjectType::OBJECT_TSDICTIONARY => "TEXT SEARCH DICTIONARY",
        ObjectType::OBJECT_TSPARSER => "TEXT SEARCH PARSER",
        ObjectType::OBJECT_TSTEMPLATE => "TEXT SEARCH TEMPLATE",
        ObjectType::OBJECT_TYPE => "TYPE",
        ObjectType::OBJECT_VIEW => "VIEW",
        ObjectType::OBJECT_MATVIEW => "MATERIALIZED VIEW",
        ObjectType::OBJECT_PUBLICATION => "PUBLICATION",
        ObjectType::OBJECT_SUBSCRIPTION => "SUBSCRIPTION",
        ObjectType::OBJECT_STATISTIC_EXT => "STATISTICS",
        ObjectType::OBJECT_TRANSFORM => "TRANSFORM",
        ObjectType::OBJECT_ACCESS_METHOD => "ACCESS METHOD",
        _ => return None,
    })
}

/// The tag of `DROP` for a kind of object, as the case `T_DropStmt` of `CreateCommandTag` gives it.
fn drop_tag(kind: ObjectType) -> String {
    let dropped = !matches!(
        kind,
        ObjectType::OBJECT_DATABASE
            | ObjectType::OBJECT_LARGEOBJECT
            | ObjectType::OBJECT_ROLE
            | ObjectType::OBJECT_TABLESPACE
            | ObjectType::OBJECT_SUBSCRIPTION
    );
    match object_word(kind).filter(|_| dropped) {
        Some(word) => format!("DROP {word}"),
        None => UNKNOWN.to_string(),
    }
}

/// `AlterObjectTypeCommandTag`: the tag of `ALTER` for a kind of object.
fn alter_tag(kind: ObjectType) -> String {
    let kind = match kind {
        ObjectType::OBJECT_ATTRIBUTE => ObjectType::OBJECT_TYPE,
        ObjectType::OBJECT_COLUMN | ObjectType::OBJECT_TABCONSTRAINT => ObjectType::OBJECT_TABLE,
        ObjectType::OBJECT_DOMCONSTRAINT => ObjectType::OBJECT_DOMAIN,
        ObjectType::OBJECT_TRANSFORM | ObjectType::OBJECT_ACCESS_METHOD => {
            return UNKNOWN.to_string();
        }
        kind => kind,
    };
    match object_word(kind) {
        Some(word) => format!("ALTER {word}"),
        None => UNKNOWN.to_string(),
    }
}
