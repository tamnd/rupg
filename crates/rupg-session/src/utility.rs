//! The statements that the session runs itself, without a plan: `SET`, `RESET`, `SHOW`, `BEGIN`, `START TRANSACTION`, `COMMIT`, `ROLLBACK` and `DISCARD`.
//!
//! Each statement follows its branch of `standard_ProcessUtility` in `utility.c`, with the rules of `ExecSetVariableStmt` and `GetPGVariable` in `guc_funcs.c` and of the transaction block functions in `xact.c`. A statement that the session cannot run yet gives the error `0A000`.

use rupg_common::{Error, SqlState};
use rupg_sql::Severity;
use rupg_sql::nodes::{
    A_Const, DiscardMode, DiscardStmt, Node, TransactionStmt, TransactionStmtKind, VariableSetKind,
    VariableSetStmt,
};
use rupg_wire::CommandTag;

use crate::block::Transaction;
use crate::guc::{self, Action, Arg, Origin, Settings};

/// A `NoticeResponse` that a statement gives before its result.
#[derive(Debug)]
pub struct Notice {
    pub severity: Severity,
    pub error: Error,
}

impl Notice {
    fn warning(error: Error) -> Notice {
        Notice { severity: Severity::Warning, error }
    }
}

/// The result of a statement.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// `CommandComplete` with this tag and no rows.
    Tag(CommandTag),
    /// Rows of text columns, then `CommandComplete` with the tag `SHOW`.
    Rows { columns: Vec<String>, rows: Vec<Vec<String>> },
}

/// What a statement can see and change.
#[allow(missing_debug_implementations)]
pub struct Context<'a> {
    pub settings: &'a mut Settings,
    pub transaction: &'a mut Transaction,
    /// The user of the session, which is the only role that rupg knows yet.
    pub user: &'a str,
    pub notices: &'a mut Vec<Notice>,
}

/// `IsTransactionExitStmt`: the statements that run in a failed transaction block.
pub fn exits_transaction(node: &Node) -> bool {
    let Node::TransactionStmt(stmt) = node else { return false };
    matches!(
        stmt.kind,
        TransactionStmtKind::TRANS_STMT_COMMIT
            | TransactionStmtKind::TRANS_STMT_PREPARE
            | TransactionStmtKind::TRANS_STMT_ROLLBACK
            | TransactionStmtKind::TRANS_STMT_ROLLBACK_TO
    )
}

/// Runs one statement. `text` is the text of the statement, for the name in the error of a statement that the session cannot run yet.
///
/// # Errors
///
/// The errors of PostgreSQL for the statement, and `0A000` for a statement that the session cannot run yet.
pub fn run(node: &Node, text: &str, cx: &mut Context<'_>) -> Result<Outcome, Error> {
    match node {
        Node::VariableSetStmt(stmt) => set(stmt, cx).map(Outcome::Tag),
        Node::VariableShowStmt(stmt) => show(stmt.name.as_deref().unwrap_or(""), cx.settings),
        Node::TransactionStmt(stmt) => transaction(stmt, cx).map(Outcome::Tag),
        Node::DiscardStmt(stmt) => discard(stmt, cx).map(Outcome::Tag),
        _ => Err(not_supported(text)),
    }
}

/// The error for a statement that the session cannot run yet, with the first word of the statement.
fn not_supported(text: &str) -> Error {
    let word: String = text
        .trim_start_matches(|c: char| c.is_whitespace() || c == '(')
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("{} is not supported yet", word.to_ascii_uppercase()),
    )
}

/// `WarnNoTransactionBlock`.
fn warn_no_block(cx: &mut Context<'_>, statement: &str) {
    if !cx.transaction.in_block() {
        cx.notices.push(Notice::warning(Error::new(
            SqlState::NO_ACTIVE_SQL_TRANSACTION,
            format!("{statement} can only be used in transaction blocks"),
        )));
    }
}

/// `RequireTransactionBlock`.
fn require_block(cx: &Context<'_>, statement: &str) -> Result<(), Error> {
    if cx.transaction.in_block() {
        return Ok(());
    }
    Err(Error::new(
        SqlState::NO_ACTIVE_SQL_TRANSACTION,
        format!("{statement} can only be used in transaction blocks"),
    ))
}

/// `PreventInTransactionBlock`.
fn prevent_block(cx: &Context<'_>, statement: &str) -> Result<(), Error> {
    if !cx.transaction.in_block() {
        return Ok(());
    }
    Err(Error::new(
        SqlState::ACTIVE_SQL_TRANSACTION,
        format!("{statement} cannot run inside a transaction block"),
    ))
}

/// One argument of `SET` as `flatten_set_variable_args` reads it: an `A_Const`, or an `A_Const` in a `TypeCast` for `SET TIME ZONE INTERVAL`. `None` is `NULL`.
fn arg(node: Option<&Node>) -> Result<Option<Arg>, Error> {
    let (node, interval) = match node {
        Some(Node::TypeCast(cast)) => (cast.arg.as_ref(), true),
        node => (node, false),
    };
    let Some(Node::A_Const(constant)) = node else {
        return Err(Error::internal("unrecognized node type in the arguments of SET"));
    };
    let A_Const { val, isnull, .. } = &**constant;
    if *isnull {
        return Ok(None);
    }
    Ok(Some(match val {
        Some(Node::Integer(value)) => Arg::Integer(value.to_string()),
        Some(Node::Float(value)) => Arg::Number(value.to_string()),
        Some(Node::String(value)) if interval => Arg::Interval(value.to_string()),
        Some(Node::String(value)) => Arg::String(value.to_string()),
        Some(Node::Boolean(value)) => Arg::String(if *value { "true" } else { "false" }.into()),
        _ => return Err(Error::internal("unrecognized constant in the arguments of SET")),
    }))
}

/// `flatten_set_variable_args`: the arguments as one value, or `None` for no arguments.
fn flatten(name: &str, args: &[Option<Node>]) -> Result<Option<String>, Error> {
    if args.is_empty() {
        return Ok(None);
    }
    let list = guc::find(name).is_some_and(|parameter| parameter.has(guc::flag::LIST_INPUT));
    if !list && args.len() > 1 {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("SET {name} takes only one argument"),
        ));
    }
    let mut values = Vec::with_capacity(args.len());
    for node in args {
        match arg(node.as_ref())? {
            Some(value) => values.push(value),
            // NULL is the empty list.
            None if list && args.len() == 1 => return Ok(Some(String::new())),
            None => {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("NULL is an invalid value for {name}"),
                ));
            }
        }
    }
    guc::flatten(name, &values).map(Some)
}

/// `SET`, `SET LOCAL`, `SET ... TO DEFAULT`, `SET ... FROM CURRENT`, `RESET`, `RESET ALL` and the special forms of `SET TRANSACTION` and `SET SESSION CHARACTERISTICS`.
fn set(stmt: &VariableSetStmt, cx: &mut Context<'_>) -> Result<CommandTag, Error> {
    let name = stmt.name.as_deref().unwrap_or("");
    let action = if stmt.is_local { Action::Local } else { Action::Set };
    match stmt.kind {
        VariableSetKind::VAR_SET_VALUE | VariableSetKind::VAR_SET_CURRENT => {
            if stmt.is_local {
                warn_no_block(cx, "SET LOCAL");
            }
            let value = if stmt.kind == VariableSetKind::VAR_SET_CURRENT {
                Some(cx.settings.show(name)?.1)
            } else {
                flatten(name, &stmt.args)?
            };
            assign(cx, name, value.as_deref(), action)?;
            Ok(CommandTag::Set)
        }
        VariableSetKind::VAR_SET_MULTI => {
            multi(stmt, cx)?;
            Ok(CommandTag::Set)
        }
        VariableSetKind::VAR_SET_DEFAULT | VariableSetKind::VAR_RESET => {
            if stmt.kind == VariableSetKind::VAR_SET_DEFAULT && stmt.is_local {
                warn_no_block(cx, "SET LOCAL");
            }
            assign(cx, name, None, action)?;
            Ok(if stmt.kind == VariableSetKind::VAR_RESET {
                CommandTag::Reset
            } else {
                CommandTag::Set
            })
        }
        VariableSetKind::VAR_RESET_ALL => {
            cx.settings.reset_all();
            Ok(CommandTag::Reset)
        }
        _ => Err(Error::internal("unexpected kind of SET")),
    }
}

/// `set_config_option` for a statement, with the checks of the parameters that need more than the settings know.
fn assign(
    cx: &mut Context<'_>,
    name: &str,
    value: Option<&str>,
    action: Action,
) -> Result<(), Error> {
    if let Some(value) = value {
        let parameter = guc::find(name);
        match parameter.map(|parameter| parameter.name) {
            // check_role and check_session_authorization: the session user is the only role yet.
            Some("role") if value != "none" && value != cx.user => {
                return Err(no_role(value));
            }
            Some("session_authorization") if value != cx.user => {
                return Err(no_role(value));
            }
            _ => {}
        }
    }
    cx.settings.set(name, value, action, Origin::Statement)
}

fn no_role(name: &str) -> Error {
    Error::new(SqlState::INVALID_PARAMETER_VALUE, format!("role \"{name}\" does not exist"))
}

/// `SET TRANSACTION`, `SET SESSION CHARACTERISTICS AS TRANSACTION` and `SET TRANSACTION SNAPSHOT`.
fn multi(stmt: &VariableSetStmt, cx: &mut Context<'_>) -> Result<(), Error> {
    let name = stmt.name.as_deref().unwrap_or("");
    let action = if stmt.is_local { Action::Local } else { Action::Set };
    let (prefix, statement) = match name {
        "TRANSACTION" => ("", "SET TRANSACTION"),
        "SESSION CHARACTERISTICS" => ("default_", ""),
        "TRANSACTION SNAPSHOT" => {
            if stmt.is_local {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "SET LOCAL TRANSACTION SNAPSHOT is not implemented",
                ));
            }
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "SET TRANSACTION SNAPSHOT is not supported yet",
            ));
        }
        _ => return Err(Error::internal(format!("unexpected SET MULTI element: {name}"))),
    };
    if !statement.is_empty() {
        warn_no_block(cx, statement);
    }
    options(&stmt.args, prefix, action, cx)
}

/// The `transaction_isolation`, `transaction_read_only` and `transaction_deferrable` items of `SET TRANSACTION`, `SET SESSION CHARACTERISTICS` and `BEGIN`, each set as the parameter with `prefix` before its name.
fn options(
    items: &[Option<Node>],
    prefix: &str,
    action: Action,
    cx: &mut Context<'_>,
) -> Result<(), Error> {
    for item in items {
        let Some(Node::DefElem(item)) = item else { continue };
        let defname = item.defname.as_deref().unwrap_or("");
        if !matches!(
            defname,
            "transaction_isolation" | "transaction_read_only" | "transaction_deferrable"
        ) {
            return Err(Error::internal(format!("unexpected transaction element: {defname}")));
        }
        let name = format!("{prefix}{defname}");
        let value = flatten(&name, std::slice::from_ref(&item.arg))?;
        cx.settings.set(&name, value.as_deref(), action, Origin::Statement)?;
    }
    Ok(())
}

/// `SHOW name` and `SHOW ALL`, as `GetPGVariable` gives them.
fn show(name: &str, settings: &Settings) -> Result<Outcome, Error> {
    if name.eq_ignore_ascii_case("all") {
        let rows = settings
            .show_all()
            .map(|(name, value, description)| vec![name.to_owned(), value, description.to_owned()])
            .collect();
        let columns = ["name", "setting", "description"].map(str::to_owned).to_vec();
        return Ok(Outcome::Rows { columns, rows });
    }
    let (column, value) = settings.show(name)?;
    Ok(Outcome::Rows { columns: vec![column], rows: vec![vec![value]] })
}

/// `BEGIN`, `START TRANSACTION`, `COMMIT`, `END`, `ROLLBACK`, `ABORT` and the forms that need savepoints or two phase commit, which the session does not have yet.
fn transaction(stmt: &TransactionStmt, cx: &mut Context<'_>) -> Result<CommandTag, Error> {
    match stmt.kind {
        TransactionStmtKind::TRANS_STMT_BEGIN | TransactionStmtKind::TRANS_STMT_START => {
            if let Some(warning) = cx.transaction.begin() {
                cx.notices.push(Notice::warning(warning));
            }
            options(&stmt.options, "", Action::Local, cx)?;
            Ok(if stmt.kind == TransactionStmtKind::TRANS_STMT_BEGIN {
                CommandTag::Begin
            } else {
                CommandTag::StartTransaction
            })
        }
        TransactionStmtKind::TRANS_STMT_COMMIT => {
            let (commits, warning) = cx.transaction.commit(stmt.chain)?;
            if let Some(warning) = warning {
                cx.notices.push(Notice::warning(warning));
            }
            Ok(if commits { CommandTag::Commit } else { CommandTag::Rollback })
        }
        TransactionStmtKind::TRANS_STMT_ROLLBACK => {
            if let Some(warning) = cx.transaction.rollback(stmt.chain)? {
                cx.notices.push(Notice::warning(warning));
            }
            Ok(CommandTag::Rollback)
        }
        TransactionStmtKind::TRANS_STMT_SAVEPOINT => {
            require_block(cx, "SAVEPOINT")?;
            Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "SAVEPOINT is not supported yet"))
        }
        TransactionStmtKind::TRANS_STMT_RELEASE => {
            require_block(cx, "RELEASE SAVEPOINT")?;
            Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "RELEASE SAVEPOINT is not supported yet",
            ))
        }
        TransactionStmtKind::TRANS_STMT_ROLLBACK_TO => {
            require_block(cx, "ROLLBACK TO SAVEPOINT")?;
            Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "ROLLBACK TO SAVEPOINT is not supported yet",
            ))
        }
        _ => Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "prepared transactions are not supported yet",
        )),
    }
}

/// `DISCARD ALL`, `DISCARD PLANS`, `DISCARD SEQUENCES` and `DISCARD TEMP`. The session has no plans, sequences or temporary tables yet, so only `DISCARD ALL` changes something: it resets the session authorization and all the settings.
fn discard(stmt: &DiscardStmt, cx: &mut Context<'_>) -> Result<CommandTag, Error> {
    Ok(match stmt.target {
        DiscardMode::DISCARD_ALL => {
            prevent_block(cx, "DISCARD ALL")?;
            cx.settings.set("session_authorization", None, Action::Set, Origin::Statement)?;
            cx.settings.reset_all();
            CommandTag::DiscardAll
        }
        DiscardMode::DISCARD_PLANS => CommandTag::DiscardPlans,
        DiscardMode::DISCARD_SEQUENCES => CommandTag::DiscardSequences,
        _ => CommandTag::DiscardTemp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::Ending;

    /// Runs the cases in a thread with 8 MiB of stack, the stack of the main thread on Linux. In a debug build the action functions of the parser have large frames, and a statement needs more than the 2 MiB of a test thread.
    // The thread is not an engine task, so it uses std::thread and not the Tasks trait of rupg-platform.
    #[allow(clippy::disallowed_methods)]
    fn big_stack(cases: fn()) {
        std::thread::Builder::new().stack_size(8 << 20).spawn(cases).unwrap().join().unwrap();
    }

    struct Session {
        settings: Settings,
        transaction: Transaction,
        notices: Vec<Notice>,
    }

    impl Session {
        fn new() -> Session {
            let mut settings = Settings::new(true);
            settings.set_internal("session_authorization", "postgres").unwrap();
            settings.share();
            Session { settings, transaction: Transaction::default(), notices: Vec::new() }
        }

        /// Runs one statement as a `Query` of one statement.
        fn run(&mut self, text: &str) -> Result<Outcome, Error> {
            let (list, _) = rupg_sql::parse(text).map_err(Error::from)?;
            let Some(Some(Node::RawStmt(raw))) = list.first() else { panic!("no statement") };
            let node = raw.stmt.as_ref().unwrap();
            self.notices.clear();
            if self.transaction.start_command() {
                self.settings.start_transaction(None);
            }
            if self.transaction.failed() && !exits_transaction(node) {
                self.transaction.abort_current();
                return Err(Error::new(SqlState::IN_FAILED_SQL_TRANSACTION, "aborted"));
            }
            let mut cx = Context {
                settings: &mut self.settings,
                transaction: &mut self.transaction,
                user: "postgres",
                notices: &mut self.notices,
            };
            let outcome = run(node, text, &mut cx);
            match &outcome {
                Ok(_) => {
                    let (ending, chained) = self.transaction.finish_command();
                    let kept = chained.then(|| self.settings.characteristics());
                    if ending != Ending::None {
                        self.settings.end(ending == Ending::Commit);
                    }
                    if kept.is_some() {
                        self.settings.start_transaction(kept);
                    }
                }
                Err(_) => {
                    if self.transaction.abort_current() == Ending::Rollback {
                        self.settings.end(false);
                    }
                }
            }
            outcome
        }

        fn show(&mut self, name: &str) -> String {
            match self.run(&format!("SHOW {name}")).unwrap() {
                Outcome::Rows { mut rows, .. } => rows.remove(0).remove(0),
                outcome => panic!("{outcome:?}"),
            }
        }

        fn state(&mut self, text: &str) -> String {
            self.run(text).unwrap_err().state().as_str().to_owned()
        }

        fn warnings(&self) -> Vec<&str> {
            self.notices.iter().map(|notice| notice.error.message()).collect()
        }
    }

    #[test]
    fn set_reset_and_show() {
        big_stack(set_reset_and_show_cases);
    }

    fn set_reset_and_show_cases() {
        let mut s = Session::new();
        assert_eq!(s.run("SET work_mem TO '8MB'").unwrap(), Outcome::Tag(CommandTag::Set));
        assert_eq!(s.show("work_mem"), "8MB");
        assert_eq!(s.run("RESET work_mem").unwrap(), Outcome::Tag(CommandTag::Reset));
        assert_eq!(s.show("work_mem"), "4MB");
        s.run("SET search_path = a, \"B\", public").unwrap();
        assert_eq!(s.show("search_path"), "a, \"B\", public");
        s.run("SET search_path = NULL").unwrap();
        assert_eq!(s.show("search_path"), "");
        assert_eq!(s.state("SET work_mem = NULL"), "22023");
        s.run("SET TIME ZONE INTERVAL '+05:30' HOUR TO MINUTE").unwrap();
        assert_eq!(s.show("TimeZone"), "<+05:30>-05:30");
        s.run("SET TIME ZONE LOCAL").unwrap();
        assert_eq!(s.show("timezone"), "GMT");
        s.run("SET datestyle FROM CURRENT").unwrap();
        assert_eq!(s.state("SHOW nothing_here"), "42704");
        assert_eq!(s.state("SET SESSION AUTHORIZATION alice"), "22023");
        s.run("SET SESSION AUTHORIZATION postgres").unwrap();
        s.run("SET ROLE none").unwrap();
        assert_eq!(s.state("SET client_encoding = 'LATIN1'"), "0A000");
        s.run("SET NAMES 'SQL_ASCII'").unwrap();
        assert_eq!(s.state("SELECT 1"), "0A000");
        assert_eq!(s.run("SELECT 1").unwrap_err().message(), "SELECT is not supported yet");
        let Outcome::Rows { columns, rows } = s.run("SHOW ALL").unwrap() else { panic!() };
        assert_eq!(columns, ["name", "setting", "description"]);
        assert!(rows.iter().any(|row| row[0] == "work_mem"));
    }

    #[test]
    fn set_local_and_transactions() {
        big_stack(set_local_and_transactions_cases);
    }

    fn set_local_and_transactions_cases() {
        let mut s = Session::new();
        s.run("SET LOCAL work_mem = '1MB'").unwrap();
        assert_eq!(s.warnings(), ["SET LOCAL can only be used in transaction blocks"]);
        assert_eq!(s.show("work_mem"), "4MB");
        assert_eq!(s.run("BEGIN").unwrap(), Outcome::Tag(CommandTag::Begin));
        s.run("SET LOCAL work_mem = '1MB'").unwrap();
        assert!(s.warnings().is_empty());
        s.run("SET statement_timeout = '5s'").unwrap();
        assert_eq!(s.show("work_mem"), "1MB");
        assert_eq!(s.run("COMMIT").unwrap(), Outcome::Tag(CommandTag::Commit));
        assert_eq!(s.show("work_mem"), "4MB");
        assert_eq!(s.show("statement_timeout"), "5s");
        s.run("START TRANSACTION").unwrap();
        s.run("SET statement_timeout = 0").unwrap();
        assert_eq!(s.run("ROLLBACK").unwrap(), Outcome::Tag(CommandTag::Rollback));
        assert_eq!(s.show("statement_timeout"), "5s");
        s.run("COMMIT").unwrap();
        assert_eq!(s.warnings(), ["there is no transaction in progress"]);
    }

    #[test]
    fn a_failed_block() {
        big_stack(a_failed_block_cases);
    }

    fn a_failed_block_cases() {
        let mut s = Session::new();
        s.run("BEGIN").unwrap();
        s.run("SET work_mem = '1MB'").unwrap();
        assert_eq!(s.state("SET work_mem = 'lots'"), "22023");
        assert_eq!(s.state("SHOW work_mem"), "25P02");
        assert_eq!(s.run("COMMIT").unwrap(), Outcome::Tag(CommandTag::Rollback));
        assert_eq!(s.show("work_mem"), "4MB");
        assert_eq!(s.state("SAVEPOINT a"), "25P01");
    }

    #[test]
    fn transaction_characteristics() {
        big_stack(transaction_characteristics_cases);
    }

    fn transaction_characteristics_cases() {
        let mut s = Session::new();
        s.run("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .unwrap();
        assert_eq!(s.show("default_transaction_isolation"), "repeatable read");
        assert_eq!(s.show("transaction_isolation"), "repeatable read");
        s.run("SET TRANSACTION READ ONLY").unwrap();
        assert_eq!(s.warnings(), ["SET TRANSACTION can only be used in transaction blocks"]);
        assert_eq!(s.show("transaction_read_only"), "off");
        s.run("BEGIN ISOLATION LEVEL SERIALIZABLE, READ ONLY, DEFERRABLE").unwrap();
        assert_eq!(s.show("transaction_isolation"), "serializable");
        assert_eq!(s.show("transaction_read_only"), "on");
        s.run("COMMIT AND CHAIN").unwrap();
        assert_eq!(s.show("transaction_isolation"), "serializable");
        assert_eq!(s.show("transaction_deferrable"), "on");
        s.run("ROLLBACK").unwrap();
        assert_eq!(s.show("transaction_isolation"), "repeatable read");
        assert_eq!(s.state("COMMIT AND CHAIN"), "25P01");
    }

    #[test]
    fn discard_all() {
        big_stack(discard_all_cases);
    }

    fn discard_all_cases() {
        let mut s = Session::new();
        s.run("SET work_mem = '1MB'").unwrap();
        assert_eq!(s.run("DISCARD ALL").unwrap(), Outcome::Tag(CommandTag::DiscardAll));
        assert_eq!(s.show("work_mem"), "4MB");
        s.run("BEGIN").unwrap();
        assert_eq!(s.state("DISCARD ALL"), "25001");
    }
}
