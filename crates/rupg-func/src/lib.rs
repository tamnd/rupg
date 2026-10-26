//! The built-in functions: the input and output functions of the types, and the kernels of the functions, the operators and the casts of `pg_proc`.
//!
//! A kernel is found by the `prosrc` of the function, as the `fmgr` of PostgreSQL finds the C function of an internal function. Many functions of the same kind, such as the comparisons and the arithmetic of the number types, share one kernel that reads the types of the call. A function that has no kernel here is an error `0A000` when the executor compiles the query, so a query never gives a wrong answer for a function that the engine does not have.

#![forbid(unsafe_code)]

mod absent;
mod acl;
mod activity;
mod advisory;
mod agg;
mod amutils;
mod array;
mod autovacuum;
mod binary;
mod bit;
mod bytea;
mod calendar;
mod cash;
mod cast;
mod catinfo;
mod compare;
mod datetime;
mod dbsize;
mod deparse;
mod describe;
mod encoding;
mod enums;
mod extract;
mod formatting;
mod generated;
mod geo;
mod hash;
mod identify;
mod inrange;
mod io;
mod json;
mod jsonbuild;
mod jsonedit;
mod jsonpath;
mod lsn;
mod mac;
mod math;
mod misc;
mod multirange;
mod network;
mod numformat;
mod random;
mod range;
mod reg;
mod regex;
mod regress;
mod replication;
mod sequence;
mod shared;
mod snapshot;
mod special;
mod srf;
mod stat;
mod text;
mod timezone;
mod trig;
mod tsearch;
mod tsheadline;
mod tsm;
mod tsmatch;
mod tsparser;
mod tsquery;
mod tsrank;
mod tsrewrite;
mod tsstat;
mod tsvector;
mod unicode;
mod updatable;
mod xml;
mod zones;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use rupg_common::{Error, Object, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{ByteaOutput, DateFormat, IntervalStyle, TimeZone, TypeError, Value};

pub use acl::{
    apply_row_security, check_execute, check_permissions, copy_from_row_security, current_user_oid,
    insert_allowed, key_shown, lock_table_allowed, row_security_hides, shown_columns,
};
pub use activity::{Backend, BackendState, Client, Ssl};
pub use agg::check_percentile;
pub use cast::local_time;
pub use compare::{compare, equal};
pub use datetime::{IntervalSum, interval_lerp};
pub use deparse::{
    deparse_constant, deparse_expression, deparse_relation_expression, explain, index_key_columns,
    partition_key_columns, view_statement,
};
pub use describe::describe;
pub use encoding::{char_to_encoding, encoding, encoding_to_char};
pub use hash::{HASH_PARTITION_SEED, extended_hash, hash_combine64, partition_hash};
pub use io::{
    base_type, input, input_soft, input_supported, is_row_type, output, output_supported, receive,
    send, to_text,
};
pub use jsonbuild::{
    is_json, json_ctor_array, json_ctor_object, json_ctor_parse, json_ctor_scalar, json_structured,
    json_value,
};
pub use jsonpath::{PathVar, SqlJsonItem, sql_json_path};
pub use multirange::range_agg;
pub use random::Randomness;
pub use reg::ts_config_setting;
pub use regex::{AuthMatch, auth_compile, auth_search};
pub use sequence::Sequences;
pub use snapshot::XactState;
pub use srf::{SetKernel, set_kernel};
pub use stat::{RelationCounts, Writes};
pub use text::quote_identifier;
pub use tsm::sample_rows;
pub use xml::{xml_expr, xml_value};

/// `format_type_be`: the name of a type for a message, with its schema when the search path does not find it.
pub fn type_message_name(ty: u32, session: &dyn Session) -> String {
    reg::type_text(ty, None, session).unwrap_or_else(|_| "???".to_string())
}

/// `errtable`: the error with the schema and the name of the relation `rel` as the names of its object. An error for a relation that is not in the catalog does not change.
pub fn with_table(error: Error, rel: u32, session: &dyn Session) -> Error {
    let Some((name, namespace)) = reg::class_name(rel, session) else { return error };
    let error = error.with_object(Object::Table, name);
    match reg::namespace_name(namespace, session) {
        Some(schema) => error.with_object(Object::Schema, schema),
        None => error,
    }
}

/// `errtablecol`: [`with_table`] and the name of the column.
pub fn with_table_column(error: Error, rel: u32, column: &str, session: &dyn Session) -> Error {
    with_table(error, rel, session).with_object(Object::Column, column)
}

/// `errtableconstraint`: [`with_table`] and the name of the constraint.
pub fn with_table_constraint(
    error: Error,
    rel: u32,
    constraint: &str,
    session: &dyn Session,
) -> Error {
    with_table(error, rel, session).with_object(Object::Constraint, constraint)
}

/// `errdatatype`: the error with the schema and the name of the type `ty` as the names of its object.
pub fn with_datatype(error: Error, ty: u32, session: &dyn Session) -> Error {
    let Some((name, namespace)) = reg::type_name(ty, session) else { return error };
    let error = error.with_object(Object::Datatype, name);
    match reg::namespace_name(namespace, session) {
        Some(schema) => error.with_object(Object::Schema, schema),
        None => error,
    }
}

/// `errdomainconstraint`: [`with_datatype`] for the domain and the name of the constraint.
pub fn with_domain_constraint(
    error: Error,
    domain: u32,
    constraint: &str,
    session: &dyn Session,
) -> Error {
    with_datatype(error, domain, session).with_object(Object::Constraint, constraint)
}

/// The OID of `internal`, the language of the functions in C.
const INTERNAL_LANGUAGE: u32 = 12;
/// The OID of the language `c`, the language of the functions of a library.
const C_LANGUAGE: u32 = 13;

/// The key of an advisory lock, the fields 2 to 4 of its tag: the two halves of a `bigint` key with `kind` 1, or two `integer` keys with `kind` 2. The database of the session is the field 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AdvisoryKey {
    pub high: u32,
    pub low: u32,
    pub kind: u16,
}

/// A request for an advisory lock: the key, the mode, which is `ShareLock` when `share` is true and `ExclusiveLock` when it is false, and the level, which is the session when `session` is true and the transaction when it is false.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdvisoryLock {
    pub key: AdvisoryKey,
    pub share: bool,
    pub session: bool,
}

/// The size of the heap of a table, `RelationGetNumberOfBlocks`, and `relpages` and `reltuples` of the table, which `VACUUM`, `ANALYZE` and `CREATE INDEX` set.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeapSize {
    pub blocks: u32,
    pub relpages: i32,
    pub reltuples: f32,
}

/// The state of the session that the functions read.
pub trait Session {
    /// The `DateStyle` setting.
    fn date_format(&self) -> DateFormat;
    /// The `IntervalStyle` setting.
    fn interval_style(&self) -> IntervalStyle;
    /// The `TimeZone` setting.
    ///
    /// # Errors
    ///
    /// `22023` when the value of `TimeZone` is not a zone.
    fn zone(&self) -> Result<Rc<dyn TimeZone>>;
    /// The `extra_float_digits` setting.
    fn extra_float_digits(&self) -> i32;
    /// The `bytea_output` setting.
    fn bytea_output(&self) -> ByteaOutput;
    /// The `array_nulls` setting.
    fn array_nulls(&self) -> bool;
    /// The start of the transaction in microseconds since 2000-01-01 UTC.
    fn transaction_start(&self) -> i64;
    /// The start of the statement in microseconds since 2000-01-01 UTC.
    fn statement_start(&self) -> i64;
    /// The time now in microseconds since 2000-01-01 UTC.
    fn clock(&self) -> i64;
    /// The current user, which is the owner during a call of a `SECURITY DEFINER` function.
    fn user(&self) -> Cow<'_, str>;
    /// The session user.
    fn session_user(&self) -> &str;
    /// `SYSTEM_USER`: the method of the authentication and the identity that it checked, as `auth_method:authn_id`, or `None` when the method checked no identity, as for `trust`.
    fn system_user(&self) -> Option<String> {
        None
    }
    /// `debug_query_string`: the text of the message of the client that runs the statement, which `current_query` gives.
    fn query(&self) -> Option<String> {
        None
    }
    /// The current database.
    fn database(&self) -> &str;
    /// The schemas of `search_path` that exist, in order, without the implicit schemas.
    fn schemas(&self) -> Vec<String>;
    /// The number of the session in the table of the sessions, which names its temporary schema `pg_temp_N`, or `None` for a session that cannot have temporary objects.
    fn temp_number(&self) -> Option<i32> {
        None
    }
    /// The process ID of the session.
    fn backend_pid(&self) -> i32;
    /// `MyTriggerDepth`: the number of the trigger functions that run, one in the other. It is 0 outside of a trigger.
    fn trigger_depth(&self) -> i32 {
        0
    }
    /// The order of the rows of the joins of the plan of `query`, as `rupg_opt::join_order` gives it: the relations from the outermost loop to the innermost one, each with true when its rows come in the reverse order of the scan. `None` when the plan gives its rows in another order or the session has no planner.
    fn join_order(&self, query: &rupg_analyze::Query) -> Option<Vec<(usize, bool)>> {
        let _ = query;
        None
    }
    /// The ID of the transaction: `GetTopFullTransactionId` when `assign` is true, which gives an ID to a transaction that has none, and `GetTopFullTransactionIdIfAny` when it is false. `None` when the transaction has no ID or the session has no server.
    fn transaction_id(&self, assign: bool) -> Option<u64> {
        let _ = assign;
        None
    }
    /// `ReadNextFullTransactionId`: the ID that the next transaction gets.
    fn next_transaction_id(&self) -> u64 {
        3
    }
    /// `GetStableLatestTransactionId`: the ID of the transaction, or the ID that the next transaction gets when it has none, the same for the whole transaction.
    fn stable_transaction_id(&self) -> u32 {
        3
    }
    /// The state of the transaction `xid`, which is before [`Session::next_transaction_id`], or `None` when the session does not know it.
    fn transaction_state(&self, xid: u32) -> Option<XactState> {
        let _ = xid;
        None
    }
    /// `GetActiveSnapshot`: the snapshot of the statement as a `pg_snapshot`, or `None` when the statement has no snapshot.
    fn current_snapshot(&self) -> Option<rupg_types::Snapshot> {
        None
    }
    /// The text of `version()`.
    fn version(&self) -> String;
    /// The value of a setting as `SHOW` gives it, or `None` for a name that is not a setting.
    fn setting(&self, name: &str) -> Option<String>;
    /// `set_config_option` for `set_config`: sets a parameter for the session, or for the transaction when `local` is true, and gives the new value as `SHOW` gives it. `value` is `None` for a reset.
    ///
    /// # Errors
    ///
    /// The errors of `SET` for the name and the value.
    fn set_setting(&self, name: &str, value: Option<&str>, local: bool) -> Result<String>;
    /// `GetConfigOptionFlags` with `GUC_LIST_QUOTE` and `SplitGUCList`: the elements of the value `text` of the parameter `name` when the parameter quotes the elements of its list, or `None` for another parameter and for a name that is not a parameter.
    fn setting_list(&self, name: &str, text: &str) -> Option<Vec<String>> {
        let _ = (name, text);
        None
    }
    /// `fmgr_security_definer` for a function with settings or with `SECURITY DEFINER`: `definer`, the owner of a `SECURITY DEFINER` function, is the current user, and `ProcessGUCArray` with `GUC_ACTION_SAVE` gives the settings of `proconfig`, each one as `name=value`, until [`Session::end_function_call`]. A session with no settings does nothing.
    ///
    /// # Errors
    ///
    /// The errors of the names and of the values.
    fn start_function_call(&self, config: &[String], definer: Option<u32>) -> Result<()> {
        let _ = (config, definer);
        Ok(())
    }
    /// The end of the call of [`Session::start_function_call`]: the current user from before the call comes back, and `AtEOXact_GUC` gives the settings their values from before the call, other than the ones that a `SET` in the call changed.
    fn end_function_call(&self) {}
    /// `CHECK_FOR_INTERRUPTS`: the check for a cancel of the statement, which a long loop of the executor, such as the loop of a recursive query, does at each step. A session with no server has no cancel.
    ///
    /// # Errors
    ///
    /// `57014` when a client canceled the statement.
    fn interrupted(&self) -> Result<()> {
        Ok(())
    }
    /// `WaitLatch` with the wait event `PgSleep`: the session waits for at most `micros` microseconds. A cancel ends the wait early, and the next call of [`Session::interrupted`] gives its error.
    ///
    /// # Errors
    ///
    /// `0A000` for a session with no server, which cannot wait.
    fn sleep(&self, micros: i64) -> Result<()> {
        let _ = micros;
        Err(not_yet("a wait in a session with no server"))
    }
    /// `LockAcquire` of an advisory lock in the database of the session. With `wait`, the session waits while another session holds or waits for a lock that conflicts, and then gives true. Without `wait`, it gives false at once.
    ///
    /// # Errors
    ///
    /// `57014` for a cancel, `40P01` for a deadlock, `55P03` for the end of `lock_timeout`, and `0A000` for a session with no server.
    fn advisory_lock(&self, lock: AdvisoryLock, wait: bool) -> Result<bool> {
        let _ = (lock, wait);
        Err(not_yet("an advisory lock in a session with no server"))
    }
    /// `LockRelease` of an advisory lock of the session level. It gives false when the session does not hold the lock in this mode at the session level.
    ///
    /// # Errors
    ///
    /// `0A000` for a session with no server.
    fn advisory_unlock(&self, lock: AdvisoryLock) -> Result<bool> {
        let _ = lock;
        Err(not_yet("an advisory lock in a session with no server"))
    }
    /// `LockReleaseSession`: releases all the advisory locks of the session level.
    ///
    /// # Errors
    ///
    /// `0A000` for a session with no server.
    fn advisory_unlock_all(&self) -> Result<()> {
        Err(not_yet("an advisory lock in a session with no server"))
    }
    /// `Async_Notify`: the transaction sends a notification on the channel `channel` with the payload `payload` when it commits.
    ///
    /// # Errors
    ///
    /// `22023` for a channel name or a payload that is not valid, and `0A000` for a session with no server.
    fn notify(&self, channel: &str, payload: &str) -> Result<()> {
        let _ = (channel, payload);
        Err(not_yet("NOTIFY in a session with no server"))
    }
    /// The channels that the session listens on, for `pg_listening_channels`. The order is not defined.
    fn listening_channels(&self) -> Vec<String> {
        Vec::new()
    }
    /// The rows of `pg_show_all_settings`, one for each parameter that `SHOW ALL` lists, in the 17 columns of the function.
    fn all_settings(&self) -> Vec<Vec<Value>> {
        Vec::new()
    }
    /// The rows of `pg_prepared_statement`, one for each named prepared statement of the session, in the 8 columns of the function. The order is not defined.
    fn prepared_statements(&self) -> Vec<Vec<Value>> {
        Vec::new()
    }
    /// The rows of `pg_cursor`, one for each cursor of `DECLARE` that is open in the session, in the 6 columns of the function. The order is not defined.
    fn cursors(&self) -> Vec<Vec<Value>> {
        Vec::new()
    }
    /// The rows of `pg_hba_file_rules`, one for each line of the host rules, in the 11 columns of the function. A session with no server has no rules and no rows.
    ///
    /// # Errors
    ///
    /// The error of a file of the rules that does not open.
    fn hba_rules(&self) -> Result<Vec<Vec<Value>>> {
        Ok(Vec::new())
    }
    /// The rows of `pg_ident_file_mappings`, one for each line of the user maps, in the 7 columns of the function. A session with no server has no maps and no rows.
    ///
    /// # Errors
    ///
    /// The error of a file of the maps that does not open.
    fn ident_mappings(&self) -> Result<Vec<Vec<Value>>> {
        Ok(Vec::new())
    }
    /// The rows of `pg_show_all_file_settings`, one for each line of the configuration files and each error in them, in the 7 columns of the function. A session with no server has no rows.
    fn file_settings(&self) -> Vec<Vec<Value>> {
        Vec::new()
    }
    /// The stored secret of the password of a role, which `pg_authid.rolpassword` shows, or `None` for a role with no password.
    fn role_password(&self, role: u32) -> Option<String> {
        let _ = role;
        None
    }
    /// The row of the file of a sequence as the sessions changed it, or `None` when no session changed it, so that the row is the one of the file when it was made.
    fn sequence_row(&self, file: u32) -> Option<rupg_catalog::SequenceData> {
        let _ = file;
        None
    }
    /// The change of the row of the file of a sequence, outside of the transaction, so that a rollback does not undo it. `initial` is the row when no session changed it yet. The change stays only when `change` gives no error, and no other session changes the row at the same time.
    ///
    /// # Errors
    ///
    /// The error of `change`, and `0A000` for a session with no server.
    fn change_sequence(
        &self,
        file: u32,
        initial: rupg_catalog::SequenceData,
        change: &mut dyn FnMut(&mut rupg_catalog::SequenceData) -> Result<()>,
    ) -> Result<()> {
        let _ = (file, initial, change);
        Err(not_yet("a sequence in a session with no server"))
    }
    /// The cache of the sequences of the session, or `None` for a session with no server.
    fn local_sequences(&self) -> Option<&std::cell::RefCell<Sequences>> {
        None
    }
    /// The state of `random` and `uuidv7` of the session, or `None` for a session with no server, which uses a state of its thread.
    fn randomness(&self) -> Option<&std::cell::RefCell<Randomness>> {
        None
    }
    /// `pg_strong_random`: fills `buf` with random bytes. A session with no server takes them from a generator with a seed from the clock.
    fn strong_random(&self, buf: &mut [u8]) {
        random::clock_random(self.clock(), buf);
    }
    /// The catalog of the user objects that the statement sees, or `None` when the session has no catalog.
    fn catalog(&self) -> Option<&rupg_catalog::Catalog> {
        None
    }
    /// The rows of the transition table `name` of the trigger that runs, which a scan of a `NamedTuplestoreScan` reads, or `None` when the trigger has no such table.
    fn transition_rows(&self, name: &str) -> Option<Rc<Vec<Vec<Value>>>> {
        let _ = name;
        None
    }
    /// The rows of `pg_database` of the databases that `CREATE DATABASE` made, in the order of their OIDs. The rows of `initdb` are static rows. A session with no server has no other databases.
    fn databases(&self) -> Arc<[rupg_catalog::Database]> {
        Arc::from(Vec::new())
    }
    /// The rows of `pg_tablespace` of the tablespaces that `CREATE TABLESPACE` made, in the order of their OIDs. The rows of `pg_default` and `pg_global` are static rows. A session with no server has no other tablespaces.
    fn tablespaces(&self) -> Arc<[rupg_catalog::Tablespace]> {
        Arc::from(Vec::new())
    }
    /// The rows of `pg_db_role_setting`: the OID of the database, the OID of the role, and the settings, each as `name=value`. A session with no server has no rows.
    fn db_role_settings(&self) -> Vec<(u32, u32, Vec<String>)> {
        Vec::new()
    }
    /// The rows of the table of the sessions in the order of their numbers, from the snapshot of the transaction. The first call of a transaction takes the snapshot.
    fn backends(&self) -> Arc<[Backend]> {
        Arc::from(Vec::new())
    }
    /// `BackendPidGetProc` and `GetBlockerStatusData`: the process IDs of the sessions that hold or wait for a lock that the session `pid` waits for, or for whose transaction it waits. A session with no server has no waits.
    fn blocking_pids(&self, pid: i32) -> Vec<i32> {
        let _ = pid;
        Vec::new()
    }
    /// `pgstat_clear_snapshot`: drops the snapshot of the table of the sessions and of the statistics.
    fn clear_backends(&self) {}
    /// `pgstat_count_heap_scan`: counts a sequential scan of a relation in the counts of the transaction. A session with no server keeps no counts.
    fn count_scan(&self, relation: u32) {
        let _ = relation;
    }
    /// The rows of a table of the user that the snapshot of the statement sees, each with a value for each column of the table when the row was written. A session with no server has no rows.
    ///
    /// # Errors
    ///
    /// An error of the store of the rows.
    fn table_rows(&self, relation: u32) -> Result<Vec<Vec<Value>>> {
        let _ = relation;
        Ok(Vec::new())
    }
    /// The rows of [`Session::table_rows`], each with the system columns `xmin` and `xmax` of its version before the ID of the version.
    ///
    /// # Errors
    ///
    /// An error of the store of the rows.
    fn table_rows_with_xids(&self, relation: u32) -> Result<Vec<Vec<Value>>> {
        let _ = relation;
        Ok(Vec::new())
    }
    /// `RelationGetNumberOfBlocks` of the heap of a table of the database of the session, with `relpages` and `reltuples` of the table, or `None` when the session has no rows.
    fn heap_size(&self, relation: u32) -> Option<HeapSize> {
        let _ = relation;
        None
    }
    /// `pgstat_fetch_stat_tabentry`: the counters of a relation of the database of the session, from the snapshot of the transaction, or `None` for a relation with no entry. The first call of a transaction takes the snapshot.
    fn relation_counts(&self, relation: u32) -> Option<RelationCounts> {
        let _ = relation;
        None
    }
    /// `find_tabstat_entry`: the sequential scans of a relation that the transaction counted and did not report yet.
    fn pending_scans(&self, relation: u32) -> i64 {
        let _ = relation;
        0
    }
    /// `find_tabstat_entry`: the rows that the transaction wrote in a relation and did not report yet, with the rows of the subtransactions that rolled back.
    fn pending_writes(&self, relation: u32) -> Writes {
        let _ = relation;
        Writes::default()
    }
    /// The time of the last reset of the statistics of the whole server in microseconds since 2000-01-01 UTC, or 0 when the session has no server.
    fn stats_reset(&self) -> i64 {
        0
    }
    /// The two ends of the connection of the session: the client, then the server.
    fn addresses(&self) -> (Client, Client) {
        (Client::Unknown, Client::Unknown)
    }
    /// `domain_check_safe` for a value of a domain or for each element of an array of a domain: the error of the first check constraint that rejects the value, or `None` when the value passes. The engine evaluates the checks, so a session without an engine cannot do this.
    ///
    /// # Errors
    ///
    /// The error of a check expression.
    fn input_checks(&self, ty: u32, value: &Value) -> Result<Option<Error>> {
        let _ = (ty, value);
        Err(not_yet("the check constraints of a domain"))
    }
    /// `ereport(WARNING)` or `ereport(NOTICE)` in a function: a message for the client, such as the warning of the input of an `aclitem` value with no grantor. A message with a SQLSTATE of class `00` or with the mark of `Error::as_notice` is a `NOTICE`, and any other message is a `WARNING`. A session with no client drops it.
    fn warn(&self, warning: Error) {
        let _ = warning;
    }
    /// `SPI_cursor_open` of a read-only plan for `text` and the fetch of all its rows, for a function that runs a query, such as `ts_stat`. It gives the type of each column and the rows.
    ///
    /// # Errors
    ///
    /// `42P11` when the text is not one statement that gives rows, the errors of the query, and `0A000` for a session that cannot run a query.
    fn run_query(&self, text: &str) -> Result<(Vec<u32>, Vec<Vec<Value>>)> {
        let _ = text;
        Err(not_yet("a query in a function"))
    }
    /// `execCurrentOf`: the `tableoid` and the `ctid` of the row of the table `table` that the cursor `cursor` is on.
    ///
    /// # Errors
    ///
    /// `34000` when there is no such cursor, and `24000` when the cursor is not on a row of the table.
    fn current_of(&self, cursor: &str, table: u32) -> Result<(Value, Value)> {
        let _ = table;
        Err(Error::new(SqlState::UNDEFINED_CURSOR, format!("cursor \"{cursor}\" does not exist")))
    }
    /// `fmgr_sql` for a function in SQL whose body is [`rupg_analyze::Body::Statements`]: a call of the function `func` with the values `args` of the types `types`. The statements run one after the other, and the call gives the types of the columns of the last statement and its rows.
    ///
    /// # Errors
    ///
    /// The errors of the statements, and `0A000` for a session that cannot run them.
    fn call_sql_function(
        &self,
        func: u32,
        types: &[u32],
        args: &[Value],
    ) -> Result<(Vec<u32>, Vec<Vec<Value>>)> {
        let _ = (func, types, args);
        Err(not_yet("a function in SQL with more than one statement"))
    }
    /// `plpgsql_call_handler`: a call of the function `func` in PL/pgSQL with the values `args` of the types `types`, for the result type `ret`. It gives the value of the function, or a value for each row of a function that returns a set. A row is a `Record`.
    ///
    /// # Errors
    ///
    /// The errors of the function, and `0A000` for a session that cannot run a function in PL/pgSQL.
    fn call_function(
        &self,
        func: u32,
        types: &[u32],
        ret: u32,
        args: &[Value],
    ) -> Result<Vec<Value>> {
        let _ = (func, types, ret, args);
        Err(not_yet("a function in PL/pgSQL"))
    }
}

/// A call of a function.
pub struct Call<'a> {
    pub session: &'a dyn Session,
    /// The types of the arguments as the query gives them.
    pub args: &'a [u32],
    /// The type of the result.
    pub ret: u32,
    /// True when the last argument is the array of a `VARIADIC` call.
    pub variadic: bool,
    /// The number of rows that the caller reads from a set kernel, or `None` for all the rows. A function of the target list stops after them, as a function in the value-per-call mode of PostgreSQL gives no more rows when the node above it stops.
    pub want: Option<usize>,
}

impl std::fmt::Debug for Call<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Call")
            .field("args", &self.args)
            .field("ret", &self.ret)
            .field("variadic", &self.variadic)
            .field("want", &self.want)
            .finish_non_exhaustive()
    }
}

/// The code of a function. It gets the values of the arguments. A strict function does not get a null value, because the executor gives null without the call.
pub type Kernel = fn(&Call<'_>, &[Value]) -> Result<Value>;

/// The error of a type function as the session sends it.
pub fn type_error(error: TypeError) -> Error {
    let mut out = Error::new(error.sqlstate, error.message);
    if let Some(detail) = error.detail {
        out = out.with_detail(detail);
    }
    if let Some(hint) = error.hint {
        out = out.with_hint(hint);
    }
    if let Some(context) = error.context {
        out = out.with_context(context);
    }
    out
}

/// The error of a feature that the engine does not have yet.
pub(crate) fn not_yet(what: impl std::fmt::Display) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet"))
}

/// The error of a value of a different kind than the type of the call gives, which is a bug of the engine.
pub(crate) fn bad_value() -> Error {
    Error::internal("a function got a value of the wrong kind")
}

/// The body of a function in SQL of the catalog of the session, as the analyzer reads it for a call with arguments of the types `inputs`, with `$1` as the first argument. `None` for a function that is not in the catalog of the session.
///
/// # Errors
///
/// The errors of the analysis of the body.
pub fn function_body(
    func: u32,
    inputs: &[u32],
    session: &dyn Session,
) -> Result<Option<rupg_analyze::Body>> {
    rupg_analyze::function_body(&reg::SessionEnv(session), func, inputs)
}

/// The kernel of the function with this `pg_proc` OID, or `None` if the engine does not have it.
pub fn kernel(func: u32) -> Option<Kernel> {
    let proc = builtin::proc_by_oid(func)?;
    let user = proc.oid >= rupg_catalog::FIRST_NORMAL_OID;
    if proc.lang == C_LANGUAGE && user {
        return regress::by_symbol(proc.src);
    }
    if proc.lang != INTERNAL_LANGUAGE {
        return misc::sql_function(func);
    }
    // A function in `internal` that a user makes runs the built-in function with the same `prosrc`.
    if user {
        let found =
            builtin::procs().iter().find(|p| p.lang == INTERNAL_LANGUAGE && p.src == proc.src)?;
        return kernel(found.oid);
    }
    if let Some(kernel) = by_src(proc.src).or_else(|| compare::by_src(proc.src, proc)) {
        return Some(kernel);
    }
    // The functions of the types for input and output. An input function with a pseudo-type as its result, such as `array_in`, needs the type of the value from its arguments, which the engine does not have yet.
    let pseudo = builtin::type_by_oid(proc.rettype).is_none_or(|row| row.kind == b'p');
    for row in builtin::types() {
        if row.send == func {
            return Some(io::send_kernel);
        }
        if row.output == func {
            return Some(io::output_kernel);
        }
        if row.input == func && !pseudo {
            return Some(io::input_kernel);
        }
    }
    let operator = operator_of(func)?;
    let left = proc.argtypes.first().copied().unwrap_or(0);
    let right = proc.argtypes.get(1).copied().unwrap_or(0);
    if proc.argtypes.len() == 2 {
        compare::by_operator(operator, left, right)
            .or_else(|| math::by_operator(operator, left, right, proc.rettype))
    } else {
        math::by_prefix_operator(operator, left, proc.rettype)
    }
}

/// The kernel of an internal function by its `prosrc`.
fn by_src(src: &str) -> Option<Kernel> {
    cast::by_src(src)
        .or_else(|| agg::by_src(src))
        .or_else(|| math::by_src(src))
        .or_else(|| datetime::by_src(src))
        .or_else(|| extract::by_src(src))
        .or_else(|| formatting::by_src(src))
        .or_else(|| numformat::by_src(src))
        .or_else(|| dbsize::by_src(src))
        .or_else(|| hash::by_src(src))
        .or_else(|| inrange::by_src(src))
        .or_else(|| timezone::by_src(src))
        .or_else(|| calendar::by_src(src))
        .or_else(|| random::by_src(src))
        .or_else(|| trig::by_src(src))
        .or_else(|| bytea::by_src(src))
        .or_else(|| json::by_src(src))
        .or_else(|| jsonedit::by_src(src))
        .or_else(|| jsonbuild::by_src(src))
        .or_else(|| jsonpath::by_src(src))
        .or_else(|| text::by_src(src))
        .or_else(|| unicode::by_src(src))
        .or_else(|| misc::by_src(src))
        .or_else(|| encoding::by_src(src))
        .or_else(|| cash::by_src(src))
        .or_else(|| mac::by_src(src))
        .or_else(|| bit::by_src(src))
        .or_else(|| binary::by_src(src))
        .or_else(|| geo::by_src(src))
        .or_else(|| lsn::by_src(src))
        .or_else(|| snapshot::by_src(src))
        .or_else(|| reg::by_src(src))
        .or_else(|| catinfo::by_src(src))
        .or_else(|| describe::by_src(src))
        .or_else(|| identify::by_src(src))
        .or_else(|| acl::by_src(src))
        .or_else(|| updatable::by_src(src))
        .or_else(|| amutils::by_src(src))
        .or_else(|| deparse::by_src(src))
        .or_else(|| stat::by_src(src))
        .or_else(|| array::by_src(src))
        .or_else(|| regex::by_src(src))
        .or_else(|| network::by_src(src))
        .or_else(|| range::by_src(src))
        .or_else(|| multirange::by_src(src))
        .or_else(|| tsvector::by_src(src))
        .or_else(|| tsquery::by_src(src))
        .or_else(|| tsearch::by_src(src))
        .or_else(|| tsmatch::by_src(src))
        .or_else(|| tsrank::by_src(src))
        .or_else(|| tsrewrite::by_src(src))
        .or_else(|| tsheadline::by_src(src))
        .or_else(|| activity::by_src(src))
        .or_else(|| replication::by_src(src))
        .or_else(|| shared::by_src(src))
        .or_else(|| advisory::by_src(src))
        .or_else(|| sequence::by_src(src))
        .or_else(|| enums::by_src(src))
        .or_else(|| xml::by_src(src))
}

/// The name of the operator that the function implements, if it implements one.
fn operator_of(func: u32) -> Option<&'static str> {
    static OPERATORS: OnceLock<BTreeMap<u32, &'static str>> = OnceLock::new();
    let map = OPERATORS
        .get_or_init(|| builtin::operators().iter().map(|op| (op.code, op.name)).collect());
    map.get(&func).copied()
}

#[cfg(test)]
mod tests;
