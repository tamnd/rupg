//! The statements that the engine plans and runs, which are `SELECT` lists without `FROM` for now, and what they read of the session.
//!
//! [`Reader`] gives the analyzer and the functions the settings, the user, the database, the time zone, the times of the transaction and the statement, and the process ID. The time zone is the zone of [`rupg_types::tz`] for the canonical name in the setting.

use std::borrow::Cow;
use std::cell::{Ref, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rupg_analyze::{Env, Params};

use rupg_catalog::{
    BOOTSTRAP_SUPERUSER, Catalog, ConKind, DepKind, FIRST_NORMAL_OID, RangePart, SequenceData,
};
use rupg_common::{Error, Result, SqlState};
use rupg_exec::Plan;
use rupg_func::{AdvisoryLock, Backend, Client, Randomness, Sequences};
use rupg_pgcatalog::builtin;
use rupg_platform::os::OsEntropy;
use rupg_platform::{Clock, Entropy};
use rupg_sql::nodes::{Node, VariableSetKind, VariableSetStmt};
use rupg_types::{
    Array, ByteaOutput, DateFormat, DateOrder, DateStyle, IntervalStyle, Recv, TimeZone, Value,
    oid, tz,
};
use rupg_wire::{Field, Portals, Statements};

use crate::advisory::{EXCLUSIVE, Edge, Mode, Request, SHARE, Tag, mode_name};
use crate::connection::{Activity, Connection, Portal, Prepared};
use crate::guc::{self, Action, Origin, Settings};
use crate::heap::View;
use crate::param::type_error;
use crate::store::Store;
use crate::utility;

/// The relations of a join in the order of its plan, each with true when its rows come in reverse.
type JoinOrder = Vec<(usize, bool)>;

/// The schemas of a new database, from `pg_namespace.dat`.
const SCHEMAS: [(&str, u32); 3] = [("pg_catalog", 11), ("pg_toast", 99), ("public", 2200)];

/// The OID of the language `sql`.
const SQL_LANGUAGE: u32 = 14;
/// The OID of the language `plpgsql`.
const PLPGSQL_LANGUAGE: u32 = 13674;
/// The OID of the language `internal`.
const INTERNAL_LANGUAGE: u32 = 12;
/// The OID of the language `c`.
const C_LANGUAGE: u32 = 13;
/// The microseconds from 1970-01-01 to 2000-01-01, the epoch of the timestamps of PostgreSQL.
const EPOCH_SHIFT_US: i64 = 946_684_800_000_000;
/// The wait event of `pg_sleep`.
const PG_SLEEP: (&str, &str) = ("Timeout", "PgSleep");
/// How long a session in `pg_sleep` waits before it looks at its cancel flag again.
const SLEEP_SLICE_US: i64 = 10_000;
/// The wait event of a wait for an advisory lock.
const ADVISORY: (&str, &str) = ("Lock", "advisory");
/// The wait event of a wait for the lock of a relation.
const RELATION: (&str, &str) = ("Lock", "relation");
/// The wait event of a wait for the end of a transaction that changed a row.
const TRANSACTION_ID: (&str, &str) = ("Lock", "transactionid");

/// A column of a result.
#[derive(Clone, Debug)]
pub(crate) struct Column {
    pub(crate) name: String,
    pub(crate) ty: u32,
    /// `typlen` of the type.
    pub(crate) size: i16,
    pub(crate) typmod: i32,
    /// The OID of the table and the attribute number when the column is a column of a table, or zeros.
    pub(crate) origin: (u32, i16),
}

impl Column {
    /// A column of type `text`, as the utility statements give.
    pub(crate) fn text(name: String) -> Column {
        Column { name, ty: oid::TEXT, size: -1, typmod: -1, origin: (0, 0) }
    }

    /// The field of `RowDescription` in a format.
    pub(crate) fn field(&self, format: i16) -> Field<'_> {
        Field {
            name: self.name.as_bytes(),
            table: self.origin.0,
            column: self.origin.1,
            type_oid: self.ty,
            type_size: self.size,
            type_modifier: self.typmod,
            format,
        }
    }
}

/// A row of a result: the bytes of each value in the format of its column, or `None` for a null value.
pub(crate) type Row = Vec<Option<Vec<u8>>>;

/// True for a statement that the engine plans, not the session.
pub(crate) fn is_query(node: &Node) -> bool {
    matches!(
        node,
        Node::SelectStmt(select) if rupg_analyze::select_into(select).is_none()
    ) || matches!(
        node,
        Node::InsertStmt(_) | Node::UpdateStmt(_) | Node::DeleteStmt(_) | Node::MergeStmt(_)
    )
}

/// The time on the clock in microseconds since 2000-01-01 UTC.
pub(crate) fn now(clock: &dyn Clock) -> i64 {
    i64::try_from(clock.wall_us()).unwrap_or(i64::MAX).saturating_sub(EPOCH_SHIFT_US)
}

/// The old and the new rows of a write for the triggers with transition tables, as the tuplestores of `TransitionCaptureState`.
#[derive(Debug, Default)]
pub(crate) struct Captured {
    /// The table of the write.
    pub(crate) table: u32,
    pub(crate) old: Rc<Vec<Vec<Value>>>,
    pub(crate) new: Rc<Vec<Vec<Value>>>,
}

/// The transition tables of one trigger: the rows of the write with the names of `REFERENCING OLD TABLE` and `REFERENCING NEW TABLE`.
#[derive(Clone, Debug)]
pub(crate) struct Transition {
    pub(crate) rows: Rc<Captured>,
    pub(crate) old: Option<String>,
    pub(crate) new: Option<String>,
}

impl Transition {
    /// The rows of the transition table `name`.
    fn rows(&self, name: &str) -> Option<&Rc<Vec<Vec<Value>>>> {
        if self.old.as_deref() == Some(name) {
            Some(&self.rows.old)
        } else if self.new.as_deref() == Some(name) {
            Some(&self.rows.new)
        } else {
            None
        }
    }
}

/// What a statement reads of the session, and the settings that `set_config` changes.
#[derive(Debug)]
pub(crate) struct Reader<'a> {
    settings: &'a RefCell<Settings>,
    /// The current user, which `role` gives, or else the session user.
    pub(crate) user: &'a str,
    /// The session user, which `session_authorization` gives.
    session_user: &'a str,
    /// The user of the login.
    login: &'a str,
    pub(crate) database: &'a str,
    pub(crate) transaction_start: i64,
    pub(crate) statement_start: i64,
    pub(crate) clock: &'a dyn Clock,
    pub(crate) pid: i32,
    /// The catalog that the statement sees: the catalog of the last commit, or the copy of the transaction that changes it.
    pub(crate) catalog: Arc<Catalog>,
    /// The text of `TimeZone` and its zone, or the name of a zone whose rules the engine does not have yet. A call of `set_config` can change the setting, so the zone is made again when the text changes.
    zone: RefCell<(String, ZoneOf)>,
    /// The row of the session in the table of the sessions, and the store that has the table, or `None` for a reader that is not a session.
    activity: Option<(&'a Activity, &'a Store)>,
    /// The prepared statements of the session, or `None` for a reader that is not a session.
    statements: Option<&'a Statements<Arc<Prepared>>>,
    /// The portals of the session, or `None` for a reader that is not a session.
    portals: Option<&'a RefCell<Portals<Portal>>>,
    /// The flag that a `CancelRequest` sets, or `None` for a reader that is not a session.
    cancel: Option<&'a AtomicBool>,
    /// The cache of the sequences of the session, or `None` for a reader that is not a session.
    sequences: Option<&'a RefCell<Sequences>>,
    /// The state of `random` and `uuidv7` of the session, or `None` for a reader that is not a session.
    randomness: Option<&'a RefCell<Randomness>>,
    /// The warnings of the functions that the statement called, such as the input function of `aclitem`. The session sends them before the result.
    warnings: RefCell<Vec<Error>>,
    /// What the statement sees of the rows of the tables, or `None` for a reader that is not a session.
    view: Option<View>,
    /// The session, which runs the functions in PL/pgSQL, or `None` for a reader that is not a session.
    conn: Option<&'a Connection>,
    /// The transition tables of the trigger whose function runs, which the queries of the function read by their names.
    transition: Option<Transition>,
    /// The order of the rows of the joins of each query that the statement ran, by the address of the query, so that a subquery that runs for each row of the query outside it plans its joins once.
    join_orders: RefCell<BTreeMap<usize, Option<JoinOrder>>>,
}

/// The zone of a `TimeZone` value, or the value if it is not a zone.
type ZoneOf = std::result::Result<Rc<Arc<tz::Zone>>, String>;

/// Gives the domains, the row types and the functions in SQL of the catalog to the thread, so that the lookups of the types and the functions by OID and by name find them, as the syscache of PostgreSQL finds the rows of `pg_type` and `pg_proc`.
fn use_types(catalog: &Catalog) {
    let mut types: Vec<builtin::SessionType> = catalog
        .types()
        .filter(|ty| ty.oid >= FIRST_NORMAL_OID)
        .filter_map(|ty| {
            if let Some(base) = &ty.base {
                return Some(builtin::SessionType::Base(builtin::SessionBase {
                    oid: ty.oid,
                    name: ty.name.clone(),
                    namespace: ty.namespace,
                    array: ty.array,
                    array_name: catalog
                        .type_by_oid(ty.array)
                        .map(|a| a.name.clone())
                        .unwrap_or_default(),
                    defined: base.defined,
                    len: base.len,
                    by_val: base.by_val,
                    category: base.category,
                    preferred: base.preferred,
                    delim: base.delim,
                    input: base.input,
                    output: base.output,
                    receive: base.receive,
                    send: base.send,
                    modin: base.modin,
                    modout: base.modout,
                    align: base.align,
                    storage: base.storage,
                    collation: base.collation,
                    like: catalog
                        .function(base.input)
                        .filter(|f| f.lang == INTERNAL_LANGUAGE)
                        .and_then(|f| builtin::type_of_input_function(&f.src))
                        .unwrap_or(0),
                }));
            }
            let array = catalog.type_by_oid(ty.array)?;
            if ty.relation != 0 {
                return Some(builtin::SessionType::Row(builtin::SessionRowType {
                    oid: ty.oid,
                    name: ty.name.clone(),
                    array: array.oid,
                    array_name: array.name.clone(),
                    namespace: ty.namespace,
                    relation: ty.relation,
                    parents: row_parents(catalog, ty.relation),
                }));
            }
            if let Some(RangePart::Range(info)) = &ty.range {
                let multirange = catalog.type_by_oid(info.multirange)?;
                let multirange_array = catalog.type_by_oid(multirange.array)?;
                let cast = catalog
                    .casts()
                    .find(|c| c.source == ty.oid && c.target == multirange.oid)
                    .map_or(0, |c| c.oid);
                return Some(builtin::SessionType::Range(builtin::SessionRange {
                    oid: ty.oid,
                    name: ty.name.clone(),
                    array: array.oid,
                    array_name: array.name.clone(),
                    namespace: ty.namespace,
                    subtype: info.subtype,
                    collation: info.collation,
                    align: info.align,
                    multirange: multirange.oid,
                    multirange_name: multirange.name.clone(),
                    multirange_namespace: multirange.namespace,
                    multirange_array: multirange_array.oid,
                    multirange_array_name: multirange_array.name.clone(),
                    constructors: info.constructors,
                    cast,
                }));
            }
            if let Some(labels) = &ty.labels {
                return Some(builtin::SessionType::Enum(builtin::SessionEnum {
                    oid: ty.oid,
                    name: ty.name.clone(),
                    array: array.oid,
                    array_name: array.name.clone(),
                    namespace: ty.namespace,
                    labels: labels.iter().map(|l| (l.oid, l.label.clone())).collect(),
                    uncommitted: labels
                        .iter()
                        .filter(|l| catalog.label_is_uncommitted(l.oid))
                        .map(|l| l.oid)
                        .collect(),
                }));
            }
            let domain = ty.domain.as_ref()?;
            let checks = catalog
                .constraints_of_domain(ty.oid)
                .filter(|c| c.kind == ConKind::Check)
                .map(|c| (c.name.clone(), c.expr.clone().unwrap_or_default()))
                .collect();
            let rows = builtin::NewDomainRows {
                oid: ty.oid,
                name: ty.name.clone(),
                array: array.oid,
                array_name: array.name.clone(),
                namespace: ty.namespace,
                base: domain.base,
                typmod: domain.typmod,
                collation: domain.collation,
            };
            Some(builtin::SessionType::Domain(builtin::SessionDomain {
                rows,
                not_null: domain.not_null,
                checks,
            }))
        })
        .collect();
    // The constructors of a range type are part of the range type, which adds them itself.
    let constructors: BTreeSet<u32> = catalog
        .depends()
        .iter()
        .filter(|d| d.object.class == rupg_catalog::PG_PROC && d.kind == DepKind::Internal)
        .map(|d| d.object.oid)
        .collect();
    types.extend(
        catalog
            .functions()
            .filter(|f| {
                f.oid >= FIRST_NORMAL_OID
                    && !constructors.contains(&f.oid)
                    && matches!(
                        f.lang,
                        SQL_LANGUAGE | PLPGSQL_LANGUAGE | INTERNAL_LANGUAGE | C_LANGUAGE
                    )
            })
            .map(|f| {
                builtin::SessionType::Function(builtin::SessionFunction {
                    oid: f.oid,
                    name: f.name.clone(),
                    namespace: f.namespace,
                    lang: f.lang,
                    strict: f.strict,
                    retset: f.retset,
                    volatile: f.volatile,
                    cost: f.cost,
                    leakproof: f.leakproof,
                    kind: f.kind,
                    rettype: f.rettype,
                    argtypes: f.argtypes.clone(),
                    allargtypes: f.allargtypes.clone(),
                    argmodes: f.argmodes.clone(),
                    argnames: f.argnames.clone(),
                    variadic: rupg_exec::variadic_type(catalog, f),
                    argdefaults: f.argdefaults.clone(),
                    nargdefaults: f.nargdefaults,
                    src: if matches!(f.lang, INTERNAL_LANGUAGE | C_LANGUAGE) {
                        f.src.clone()
                    } else {
                        String::new()
                    },
                })
            }),
    );
    types.extend(catalog.operators().map(|o| {
        builtin::SessionType::Operator(builtin::SessionOperator {
            oid: o.oid,
            name: o.name.clone(),
            namespace: o.namespace,
            kind: o.kind,
            can_merge: o.can_merge,
            can_hash: o.can_hash,
            left: o.left,
            right: o.right,
            result: o.result,
            commutator: o.commutator,
            negator: o.negator,
            code: o.code,
        })
    }));
    // The cast of a range type to its multirange type comes with the range type.
    let made =
        catalog.casts().filter(|c| catalog.type_by_oid(c.source).is_none_or(|t| t.range.is_none()));
    types.extend(made.map(|c| {
        builtin::SessionType::Cast(builtin::SessionCast {
            oid: c.oid,
            source: c.source,
            target: c.target,
            func: c.func,
            context: c.context,
            method: c.method,
        })
    }));
    types.extend(catalog.aggregates().map(|a| {
        builtin::SessionType::Aggregate(builtin::SessionAggregate {
            fnoid: a.function,
            kind: a.kind,
            ndirect: a.ndirect,
            transfn: a.transfn,
            finalfn: a.finalfn,
            finalextra: a.finalextra,
            sortop: a.sortop,
            transtype: a.transtype,
            initval: a.initval.clone(),
            mtransfn: a.mtransfn,
            minvtransfn: a.minvtransfn,
            mfinalfn: a.mfinalfn,
            mfinalextra: a.mfinalextra,
            finalmodify: a.finalmodify,
            mfinalmodify: a.mfinalmodify,
            mtranstype: a.mtranstype,
            minitval: a.minitval.clone(),
        })
    }));
    builtin::use_session_types(types);
}

impl<'a> Reader<'a> {
    pub(crate) fn new(
        settings: &'a RefCell<Settings>,
        user: &'a str,
        database: &'a str,
        times: (i64, i64),
        clock: &'a dyn Clock,
        pid: i32,
        catalog: Arc<Catalog>,
    ) -> Reader<'a> {
        let name = settings.borrow().get("TimeZone").unwrap_or_else(|| "UTC".to_owned());
        let zone = session_zone(&name);
        use_types(&catalog);
        Reader {
            settings,
            user,
            session_user: user,
            login: user,
            database,
            transaction_start: times.0,
            statement_start: times.1,
            clock,
            pid,
            catalog,
            zone: RefCell::new((name, zone)),
            activity: None,
            statements: None,
            portals: None,
            cancel: None,
            sequences: None,
            randomness: None,
            warnings: RefCell::new(Vec::new()),
            view: None,
            conn: None,
            transition: None,
            join_orders: RefCell::new(BTreeMap::new()),
        }
    }

    /// The warnings of the functions since the last call.
    pub(crate) fn take_warnings(&self) -> Vec<Error> {
        self.warnings.take()
    }

    /// The reader of a session whose session user and user of the login are not its current user.
    pub(crate) fn with_users(mut self, session_user: &'a str, login: &'a str) -> Reader<'a> {
        self.session_user = session_user;
        self.login = login;
        self
    }

    /// The reader of a session, which reads the table of the sessions in `store`.
    pub(crate) fn with_activity(mut self, activity: &'a Activity, store: &'a Store) -> Reader<'a> {
        self.activity = Some((activity, store));
        self
    }

    /// The reader of a session with a cache of sequences.
    pub(crate) fn with_sequences(mut self, sequences: &'a RefCell<Sequences>) -> Reader<'a> {
        self.sequences = Some(sequences);
        self
    }

    /// The reader of a session with a state of `random`.
    pub(crate) fn with_randomness(mut self, randomness: &'a RefCell<Randomness>) -> Reader<'a> {
        self.randomness = Some(randomness);
        self
    }

    /// The reader of a session that a `CancelRequest` can stop.
    pub(crate) fn with_cancel(mut self, cancel: &'a AtomicBool) -> Reader<'a> {
        self.cancel = Some(cancel);
        self
    }

    /// The reader of a session with prepared statements.
    pub(crate) fn with_statements(
        mut self,
        statements: &'a Statements<Arc<Prepared>>,
    ) -> Reader<'a> {
        self.statements = Some(statements);
        self
    }

    /// The reader of a session with portals.
    pub(crate) fn with_portals(mut self, portals: &'a RefCell<Portals<Portal>>) -> Reader<'a> {
        self.portals = Some(portals);
        self
    }

    /// The reader of the session `conn`, which can run a function in PL/pgSQL.
    pub(crate) fn with_connection(mut self, conn: &'a Connection) -> Reader<'a> {
        self.conn = Some(conn);
        self
    }

    /// The reader of the queries of a trigger function with the transition tables `transition`.
    pub(crate) fn with_transition(mut self, transition: Option<Transition>) -> Reader<'a> {
        self.transition = transition;
        self
    }

    /// The warnings of the functions, which go to the warnings of the statement.
    pub(crate) fn add_warnings(&self, warnings: Vec<Error>) {
        self.warnings.borrow_mut().extend(warnings);
    }

    /// The reader of a session that sees the rows of the tables through `view`.
    pub(crate) fn with_view(mut self, view: View) -> Reader<'a> {
        self.view = Some(view);
        self
    }

    /// What the statement sees of the rows, or `None` for a reader that is not a session.
    pub(crate) fn view(&self) -> Option<&View> {
        self.view.as_ref()
    }

    /// The row of the session and its store, or `0A000` for a reader that is not a session, which has no locks and cannot wait.
    fn server(&self) -> Result<(&'a Activity, &'a Store)> {
        self.activity.ok_or_else(|| {
            Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "a lock in a session with no server is not supported yet",
            )
        })
    }

    /// A timeout setting in milliseconds.
    fn timeout(&self, name: &str) -> i64 {
        match guc::find(name).map(|parameter| self.settings().setting(parameter)) {
            Some(guc::Setting::Int(ms)) => i64::from(ms),
            _ => 0,
        }
    }

    /// `ProcSleep` for the request of the session in the queue of `tag`. After `deadlock_timeout`, the session looks once for a cycle of waits, as `CheckDeadLock` does. A cancel and the end of `lock_timeout` stop the wait.
    fn wait_lock(&self, store: &Store, tag: Tag, mode: Mode, session: bool) -> Result<()> {
        let locks = store.locks();
        let start = now(self.clock);
        let mut check = Some(start.saturating_add(self.timeout("deadlock_timeout") * 1000));
        let limit = match self.timeout("lock_timeout") {
            0 => None,
            ms => Some(start.saturating_add(ms * 1000)),
        };
        let slice = Duration::from_micros(SLEEP_SLICE_US.cast_unsigned());
        let waiting = || format!("waiting for {} on {tag}", mode_name(mode));
        loop {
            if locks.wait(tag, self.pid, session, slice, store.tasks()) {
                return Ok(());
            }
            if self.cancel.is_some_and(|c| c.swap(false, Ordering::Relaxed)) {
                return Err(Error::new(
                    SqlState::QUERY_CANCELED,
                    "canceling statement due to user request",
                )
                .with_context(waiting()));
            }
            let time = now(self.clock);
            if limit.is_some_and(|limit| time >= limit) {
                return Err(Error::new(
                    SqlState::LOCK_NOT_AVAILABLE,
                    "canceling statement due to lock timeout",
                )
                .with_context(waiting()));
            }
            if check.is_some_and(|check| time >= check) {
                check = None;
                if let Some(cycle) = locks.deadlock(self.pid) {
                    return Err(deadlock(&cycle));
                }
            }
        }
    }

    /// `LockAcquire` of `tag` in `mode` at the session level or at the transaction level. Without `wait`, a lock that is not free gives false. With `wait`, the session waits in the queue of the lock and reports the wait event `event`.
    fn acquire(
        &self,
        tag: Tag,
        mode: Mode,
        session: bool,
        wait: bool,
        event: (&'static str, &'static str),
    ) -> Result<bool> {
        let (activity, store) = self.server()?;
        let locks = store.locks();
        match locks.request(tag, self.pid, mode, session, wait) {
            Request::Granted => return Ok(true),
            Request::Busy => return Ok(false),
            Request::Deadlock(cycle) => return Err(deadlock(&cycle)),
            Request::Queued => {}
        }
        activity.report_wait(store, Some(event));
        let result = self.wait_lock(store, tag, mode, session);
        activity.report_wait(store, None);
        if result.is_err() {
            locks.leave_queue(tag, self.pid);
        }
        result.map(|()| true)
    }

    /// `LockRelationOid` in `mode`, which is 1 for `AccessShareLock` to 8 for `AccessExclusiveLock`, or `ConditionalLockRelationOid` without `wait`. The transaction holds the lock until it ends. False when the lock is not free and the session does not wait.
    ///
    /// # Errors
    ///
    /// `57014` for a cancel, `55P03` at the end of `lock_timeout` and `40P01` for a cycle of waits.
    pub(crate) fn lock_relation(&self, relation: u32, mode: Mode, wait: bool) -> Result<bool> {
        let (_, store) = self.server()?;
        let tag = Tag::Relation { database: database_oid(store, self.database), relation };
        self.acquire(tag, mode, false, wait, RELATION)
    }

    /// `XactLockTableWait`: the wait of the transaction `xid` for the end of the transaction `holder`, which changed a row that the statement changes. After `deadlock_timeout`, the session looks once for a cycle of waits. A cancel and the end of `lock_timeout` stop the wait. `context` tells the row, as `XactLockTableWaitErrorCb` does.
    ///
    /// # Errors
    ///
    /// `57014` for a cancel, `55P03` at the end of `lock_timeout` and `40P01` for a cycle of waits.
    pub(crate) fn wait_xact(&self, xid: u32, holder: u32, context: &str) -> Result<()> {
        let (activity, store) = self.server()?;
        let heap = store.heap();
        heap.wait(xid, holder);
        activity.report_wait(store, Some(TRANSACTION_ID));
        let start = now(self.clock);
        let mut check = Some(start.saturating_add(self.timeout("deadlock_timeout") * 1000));
        let limit = match self.timeout("lock_timeout") {
            0 => None,
            ms => Some(start.saturating_add(ms * 1000)),
        };
        let slice = Duration::from_micros(SLEEP_SLICE_US.cast_unsigned());
        let waiting = format!("waiting for ShareLock on transaction {holder}");
        let mut result = Ok(());
        while heap.running(holder) {
            if let Err(error) = rupg_func::Session::interrupted(self) {
                result = Err(error.with_context(waiting).with_context(context));
                break;
            }
            let time = now(self.clock);
            if limit.is_some_and(|limit| time >= limit) {
                result = Err(Error::new(
                    SqlState::LOCK_NOT_AVAILABLE,
                    "canceling statement due to lock timeout",
                )
                .with_context(waiting)
                .with_context(context));
                break;
            }
            if check.is_some_and(|check| time >= check) {
                check = None;
                if let Some(cycle) = heap.deadlock(xid) {
                    result = Err(xact_deadlock(&cycle).with_context(context));
                    break;
                }
            }
            store.nap(slice);
        }
        activity.report_wait(store, None);
        heap.waited(xid);
        result
    }

    /// The settings of the session.
    pub(crate) fn settings(&self) -> Ref<'_, Settings> {
        self.settings.borrow()
    }

    /// `GetUserId`: the name of the current user, which is the owner of the function in a call of a `SECURITY DEFINER` function.
    pub(crate) fn current_user(&self) -> Cow<'_, str> {
        match self.settings().definer() {
            Some((_, name)) => Cow::Owned(name.clone()),
            None => Cow::Borrowed(self.user),
        }
    }

    /// `GetUserNameFromId`. The user of the login is the bootstrap superuser when no role has its name.
    fn role_name(&self, role: u32) -> String {
        let roles = rupg_pgcatalog::roles::current();
        if role == BOOTSTRAP_SUPERUSER && roles.oid(self.login).is_none() {
            return self.login.to_owned();
        }
        roles.name(role).map_or_else(|| role.to_string(), str::to_owned)
    }

    /// The schemas of `search_path` that exist, in order, without the implicit schemas, as `recomputeNamespacePath` finds them. `$user` names the schema of the user, and `pg_temp` the temporary schema, which do not exist yet.
    fn path(&self) -> Vec<(String, u32)> {
        let text = self.settings().get("search_path").unwrap_or_default();
        let mut path: Vec<(String, u32)> = Vec::new();
        for name in guc::split_identifiers(&text, ',').unwrap_or_default() {
            let name = match name.as_str() {
                "$user" => self.current_user().into_owned(),
                "pg_temp" => match self.temp_name() {
                    Some(temp) => temp,
                    None => continue,
                },
                _ => name,
            };
            if let Some(oid) = self.schema(&name)
                && !path.iter().any(|&(_, o)| o == oid)
            {
                path.push((name, oid));
            }
        }
        path
    }

    /// The name of the temporary schema of the session, if it exists.
    fn temp_name(&self) -> Option<String> {
        let number = self.activity.and_then(|(activity, _)| activity.number())?;
        let name = format!("pg_temp_{number}");
        self.catalog.schema_by_name(&name).map(|_| name)
    }

    /// The OID of a built-in schema or a schema of the catalog. `pg_temp` names the temporary schema of the session.
    fn schema(&self, name: &str) -> Option<u32> {
        if name == "pg_temp" {
            return self.temp_name().and_then(|temp| self.schema(&temp));
        }
        SCHEMAS
            .iter()
            .find(|(n, oid)| *n == name && !self.catalog.schema_dropped(*oid))
            .map(|(_, oid)| *oid)
            .or_else(|| self.catalog.schema_by_name(name).map(|s| s.oid))
    }
}

/// The OID of a database, or 0 for a name that is not a database. The name of a database does not change while a session uses it.
/// The row types that the row type of `relation` can take: the row types of all the ancestors of the relation in `pg_inherits`, as `typeInheritsFrom` walks them, and `reloftype`, as `typeIsOfTypedTable` reads it.
fn row_parents(catalog: &Catalog, relation: u32) -> Vec<u32> {
    let mut parents = Vec::new();
    let mut seen = vec![relation];
    let mut queue = vec![relation];
    while let Some(oid) = queue.pop() {
        let Some(rel) = catalog.relation(oid) else {
            continue;
        };
        if oid == relation && rel.of_type != 0 {
            parents.push(rel.of_type);
        }
        let partition = rel.partition_of.as_ref().map(|p| p.parent);
        for parent in rel.inherits.iter().copied().chain(partition) {
            if parent == 0 || seen.contains(&parent) {
                continue;
            }
            seen.push(parent);
            queue.push(parent);
            if let Some(row) = catalog.relation(parent).map(|r| r.row_type)
                && row != 0
            {
                parents.push(row);
            }
        }
    }
    parents
}

fn database_oid(store: &Store, name: &str) -> u32 {
    store.database(name).map_or(0, |db| db.oid)
}

/// `DeadLockReport`: the error of a cycle of waits, with a line for each wait.
fn deadlock(cycle: &[Edge]) -> Error {
    let lines: Vec<String> = cycle
        .iter()
        .enumerate()
        .map(|(i, edge)| {
            let next = cycle[(i + 1) % cycle.len()].pid;
            format!(
                "Process {} waits for {} on {}; blocked by process {next}.",
                edge.pid,
                mode_name(edge.mode),
                edge.tag
            )
        })
        .collect();
    Error::new(SqlState::T_R_DEADLOCK_DETECTED, "deadlock detected")
        .with_detail(lines.join("\n"))
        .with_hint("See server log for query details.")
}

/// `DeadLockReport` for a cycle of waits for the ends of transactions.
fn xact_deadlock(cycle: &[crate::heap::Wait]) -> Error {
    let lines: Vec<String> = cycle
        .iter()
        .map(|wait| {
            format!(
                "Process {} waits for ShareLock on transaction {}; blocked by process {}.",
                wait.pid, wait.xid, wait.holder
            )
        })
        .collect();
    Error::new(SqlState::T_R_DEADLOCK_DETECTED, "deadlock detected")
        .with_detail(lines.join("\n"))
        .with_hint("See server log for query details.")
}

/// The zone of a `TimeZone` value that the setting accepted. The setting keeps the canonical name, which `pg_tzset` takes again.
fn session_zone(text: &str) -> ZoneOf {
    tz::load(text).map(Rc::new).ok_or_else(|| text.to_owned())
}

/// `DateStyle` in its canonical form, such as `ISO, MDY`.
fn date_format(text: &str) -> DateFormat {
    let (style, order) = text.split_once(", ").unwrap_or((text, "MDY"));
    let style = match style {
        "SQL" => DateStyle::Sql,
        "Postgres" => DateStyle::Postgres,
        "German" => DateStyle::German,
        _ => DateStyle::Iso,
    };
    let order = match order {
        "DMY" => DateOrder::Dmy,
        "YMD" => DateOrder::Ymd,
        _ => DateOrder::Mdy,
    };
    DateFormat { style, order }
}

impl rupg_func::Session for Reader<'_> {
    fn join_order(&self, query: &rupg_analyze::Query) -> Option<Vec<(usize, bool)>> {
        let (_, store) = self.activity?;
        let address = std::ptr::from_ref(query) as usize;
        if let Some(order) = self.join_orders.borrow().get(&address) {
            return order.clone();
        }
        let database = database_oid(store, self.database);
        let order = crate::connection::join_order(self, store.heap(), database, query);
        self.join_orders.borrow_mut().insert(address, order.clone());
        order
    }

    fn interrupted(&self) -> Result<()> {
        if self.cancel.is_some_and(|c| c.swap(false, Ordering::Relaxed)) {
            return Err(Error::new(
                SqlState::QUERY_CANCELED,
                "canceling statement due to user request",
            ));
        }
        Ok(())
    }

    fn sleep(&self, micros: i64) -> Result<()> {
        let Some((activity, store)) = self.activity else {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "a wait in a session with no server is not supported yet",
            ));
        };
        activity.report_wait(store, Some(PG_SLEEP));
        let end = now(self.clock).saturating_add(micros);
        while !self.cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            let left = end.saturating_sub(now(self.clock));
            if left <= 0 {
                break;
            }
            store.nap(Duration::from_micros(left.min(SLEEP_SLICE_US).cast_unsigned()));
        }
        activity.report_wait(store, None);
        Ok(())
    }

    fn advisory_lock(&self, lock: AdvisoryLock, wait: bool) -> Result<bool> {
        let (_, store) = self.server()?;
        let tag = Tag::Advisory { database: database_oid(store, self.database), key: lock.key };
        let mode = if lock.share { SHARE } else { EXCLUSIVE };
        self.acquire(tag, mode, lock.session, wait, ADVISORY)
    }

    fn advisory_unlock(&self, lock: AdvisoryLock) -> Result<bool> {
        let (_, store) = self.server()?;
        let tag = Tag::Advisory { database: database_oid(store, self.database), key: lock.key };
        let mode = if lock.share { SHARE } else { EXCLUSIVE };
        Ok(store.locks().release(tag, self.pid, mode))
    }

    fn advisory_unlock_all(&self) -> Result<()> {
        let (_, store) = self.server()?;
        store.locks().release_all(self.pid, true);
        Ok(())
    }

    fn sequence_row(&self, file: u32) -> Option<SequenceData> {
        self.activity
            .and_then(|(_, store)| store.sequence_row(database_oid(store, self.database), file))
    }

    fn change_sequence(
        &self,
        file: u32,
        initial: SequenceData,
        change: &mut dyn FnMut(&mut SequenceData) -> Result<()>,
    ) -> Result<()> {
        let Some((_, store)) = self.activity else {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "a sequence in a session with no server is not supported yet",
            ));
        };
        store.change_sequence(database_oid(store, self.database), file, initial, change)
    }

    fn local_sequences(&self) -> Option<&RefCell<Sequences>> {
        self.sequences
    }

    fn randomness(&self) -> Option<&RefCell<Randomness>> {
        let randomness = self.randomness?;
        // `assign_random_seed`: a `SET seed` seeds the generator before its next number.
        if let Some(seed) = self.settings.try_borrow_mut().ok().and_then(|mut s| s.take_seed()) {
            randomness.borrow_mut().set_seed(seed);
        }
        Some(randomness)
    }

    fn strong_random(&self, buf: &mut [u8]) {
        OsEntropy.fill(buf);
    }

    fn date_format(&self) -> DateFormat {
        self.settings().get("DateStyle").map_or(DateFormat::ISO_MDY, |text| date_format(&text))
    }

    fn interval_style(&self) -> IntervalStyle {
        match self.settings().get("IntervalStyle").as_deref() {
            Some("postgres_verbose") => IntervalStyle::PostgresVerbose,
            Some("sql_standard") => IntervalStyle::SqlStandard,
            Some("iso_8601") => IntervalStyle::Iso8601,
            _ => IntervalStyle::Postgres,
        }
    }

    fn zone(&self) -> Result<Rc<dyn TimeZone>> {
        let name = self.settings().get("TimeZone").unwrap_or_else(|| "UTC".to_owned());
        let mut zone = self.zone.borrow_mut();
        if zone.0 != name {
            let made = session_zone(&name);
            *zone = (name, made);
        }
        match &zone.1 {
            Ok(zone) => Ok(Rc::clone(zone) as Rc<dyn TimeZone>),
            Err(name) => Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("time zone \"{name}\" not recognized"),
            )),
        }
    }

    fn extra_float_digits(&self) -> i32 {
        self.settings().get("extra_float_digits").and_then(|v| v.parse().ok()).unwrap_or(1)
    }

    fn bytea_output(&self) -> ByteaOutput {
        match self.settings().get("bytea_output").as_deref() {
            Some("escape") => ByteaOutput::Escape,
            _ => ByteaOutput::Hex,
        }
    }

    fn array_nulls(&self) -> bool {
        self.settings().get("array_nulls").as_deref() != Some("off")
    }

    fn transaction_start(&self) -> i64 {
        self.transaction_start
    }

    fn statement_start(&self) -> i64 {
        self.statement_start
    }

    fn clock(&self) -> i64 {
        now(self.clock)
    }

    fn user(&self) -> Cow<'_, str> {
        self.current_user()
    }

    fn session_user(&self) -> &str {
        self.session_user
    }

    fn system_user(&self) -> Option<String> {
        self.activity.and_then(|(activity, _)| activity.system_user.clone())
    }

    fn query(&self) -> Option<String> {
        self.activity.and_then(|(activity, _)| activity.query.clone())
    }

    fn database(&self) -> &str {
        self.database
    }

    fn catalog(&self) -> Option<&Catalog> {
        Some(&self.catalog)
    }

    fn transition_rows(&self, name: &str) -> Option<Rc<Vec<Vec<Value>>>> {
        self.transition.as_ref()?.rows(name).cloned()
    }

    fn schemas(&self) -> Vec<String> {
        self.path().into_iter().map(|(name, _)| name).collect()
    }

    fn temp_number(&self) -> Option<i32> {
        self.activity.and_then(|(activity, _)| activity.number())
    }

    fn backend_pid(&self) -> i32 {
        self.pid
    }

    fn trigger_depth(&self) -> i32 {
        self.conn.map_or(0, Connection::trigger_depth)
    }

    fn transaction_id(&self, assign: bool) -> Option<u64> {
        self.conn?.transaction_id(assign).map(u64::from)
    }

    fn next_transaction_id(&self) -> u64 {
        self.conn.map_or(662, |conn| u64::from(conn.heap().next_xid()))
    }

    fn stable_transaction_id(&self) -> u32 {
        self.conn.map_or(3, Connection::stable_xid)
    }

    fn transaction_state(&self, xid: u32) -> Option<rupg_func::XactState> {
        Some(self.conn?.heap().xact_state(xid))
    }

    fn current_snapshot(&self) -> Option<rupg_types::Snapshot> {
        let view = self.view.as_ref()?;
        let own = self.conn.and_then(|conn| conn.transaction_id(false)).unwrap_or(view.xid);
        Some(view.snapshot.exported(own))
    }

    fn databases(&self) -> Arc<[rupg_catalog::Database]> {
        match self.activity {
            Some((_, store)) => store.created(),
            None => Arc::from(Vec::new()),
        }
    }

    fn tablespaces(&self) -> Arc<[rupg_catalog::Tablespace]> {
        match self.activity {
            Some((_, store)) => store.tablespaces(),
            None => Arc::from(Vec::new()),
        }
    }

    fn db_role_settings(&self) -> Vec<(u32, u32, Vec<String>)> {
        match self.activity {
            Some((_, store)) => store.db_role_settings(),
            None => Vec::new(),
        }
    }

    fn backends(&self) -> Arc<[Backend]> {
        match self.activity {
            Some((activity, store)) => activity.backends(store, &self.settings()),
            None => Arc::from(Vec::new()),
        }
    }

    fn blocking_pids(&self, pid: i32) -> Vec<i32> {
        let Some((_, store)) = self.activity else { return Vec::new() };
        let mut pids = store.locks().blockers(pid);
        let lease = store.lease_blocker(pid);
        for holder in store.heap().blockers(pid).into_iter().chain(lease) {
            if !pids.contains(&holder) {
                pids.push(holder);
            }
        }
        pids
    }

    fn clear_backends(&self) {
        if let Some((activity, _)) = self.activity {
            activity.clear();
        }
    }

    fn count_scan(&self, relation: u32) {
        if let Some((activity, _)) = self.activity
            && self.settings().get("track_counts").as_deref() != Some("off")
        {
            activity.count_scan(relation);
        }
    }

    fn table_rows(&self, relation: u32) -> Result<Vec<Vec<Value>>> {
        match (self.activity, &self.view) {
            (Some((_, store)), Some(view)) => Ok(store.heap().rows(relation, view)),
            _ => Ok(Vec::new()),
        }
    }

    fn table_rows_with_xids(&self, relation: u32) -> Result<Vec<Vec<Value>>> {
        match (self.activity, &self.view) {
            (Some((_, store)), Some(view)) => Ok(store.heap().rows_with_xids(relation, view)),
            _ => Ok(Vec::new()),
        }
    }

    fn heap_size(&self, relation: u32) -> Option<rupg_func::HeapSize> {
        let (_, store) = self.activity?;
        Some(store.heap().size(database_oid(store, self.database), relation))
    }

    fn statistics(&self, relation: u32) -> Vec<rupg_func::Statistic> {
        let Some((_, store)) = self.activity else { return Vec::new() };
        store.heap().statistics(database_oid(store, self.database), relation)
    }

    fn relation_counts(&self, relation: u32) -> Option<rupg_func::RelationCounts> {
        let (activity, store) = self.activity?;
        activity.relation_counts(store, relation)
    }

    fn pending_scans(&self, relation: u32) -> i64 {
        self.activity.map_or(0, |(activity, _)| activity.pending_scans(relation))
    }

    fn pending_writes(&self, relation: u32) -> rupg_func::Writes {
        self.activity
            .map_or_else(Default::default, |(activity, _)| activity.pending_writes(relation))
    }

    fn hba_rules(&self) -> Result<Vec<Vec<Value>>> {
        match self.activity.and_then(|(_, store)| store.auth()) {
            Some(auth) => Ok(auth.hba_rules()?.into_iter().map(crate::auth::hba_values).collect()),
            None => Ok(Vec::new()),
        }
    }

    fn file_settings(&self) -> Vec<Vec<Value>> {
        let Some(auth) = self.activity.and_then(|(_, store)| store.auth()) else {
            return Vec::new();
        };
        let lines = auth.file_settings(&self.settings());
        lines
            .into_iter()
            .enumerate()
            .map(|(i, line)| crate::auth::file_values(i + 1, line))
            .collect()
    }

    fn ident_mappings(&self) -> Result<Vec<Vec<Value>>> {
        match self.activity.and_then(|(_, store)| store.auth()) {
            Some(auth) => {
                Ok(auth.ident_mappings()?.into_iter().map(crate::auth::ident_values).collect())
            }
            None => Ok(Vec::new()),
        }
    }

    fn role_password(&self, role: u32) -> Option<String> {
        let (_, store) = self.activity?;
        // `BOOTSTRAP_SUPERUSERID`: the server has one role with a password.
        if role == 10 { store.password().map(str::to_owned) } else { None }
    }

    fn stats_reset(&self) -> i64 {
        self.activity.map_or(0, |(_, store)| store.stats_reset())
    }

    fn addresses(&self) -> (Client, Client) {
        self.activity
            .map_or_else(Default::default, |(activity, _)| (activity.client, activity.server))
    }

    fn input_checks(&self, ty: u32, value: &Value) -> Result<Option<Error>> {
        rupg_exec::input_failure(ty, value, self)
    }

    fn warn(&self, warning: Error) {
        self.warnings.borrow_mut().push(warning);
    }

    fn call_function(
        &self,
        func: u32,
        types: &[u32],
        ret: u32,
        args: &[Value],
    ) -> Result<Vec<Value>> {
        match self.conn {
            Some(conn) => conn.pl_call(self, func, types, ret, args),
            None => Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "a function in PL/pgSQL is not supported here",
            )),
        }
    }

    fn call_sql_function(
        &self,
        func: u32,
        types: &[u32],
        args: &[Value],
    ) -> Result<(Vec<u32>, Vec<Vec<Value>>)> {
        match self.conn {
            Some(conn) => conn.sql_call(self, func, types, args),
            None => Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "a function in SQL with more than one statement is not supported here",
            )),
        }
    }

    fn run_query(&self, text: &str) -> Result<(Vec<u32>, Vec<Vec<Value>>)> {
        let (list, _) = rupg_sql::parse(text)?;
        let statements: Vec<&Node> = list
            .iter()
            .flatten()
            .filter_map(|node| match node {
                Node::RawStmt(raw) => raw.stmt.as_ref(),
                _ => None,
            })
            .collect();
        let prepare = |node: &Node| {
            let start = self.warnings.borrow().len();
            let plan = plan(node, self, &Params::default()).and_then(|plan| {
                for warning in plan.notices() {
                    self.warn(warning.clone());
                }
                plan.fold(None, self)
            });
            // The positions of the messages are in the text of the inner query, which the client does not have.
            for warning in &mut self.warnings.borrow_mut()[start..] {
                *warning = warning.clone().with_position(None);
            }
            plan.map_err(|error| inner_error(error, text))
        };
        let [node] = statements.as_slice() else {
            // `SPI_prepare` plans all the statements before `SPI_cursor_open` looks at their number.
            for node in statements.into_iter().filter(|node| is_query(node)) {
                prepare(node)?;
            }
            return Err(Error::new(
                SqlState::INVALID_CURSOR_DEFINITION,
                "cannot open multi-query plan as cursor",
            ));
        };
        if let Some(error) = cursor_error(node, text) {
            return Err(error);
        }
        let plan = prepare(node)?;
        let types = plan.columns().iter().map(|target| target.expr.ty).collect();
        Ok((types, plan.run(&[], self)?))
    }

    fn version(&self) -> String {
        crate::connection::version_text()
    }

    fn setting(&self, name: &str) -> Option<String> {
        self.settings().get(name)
    }

    fn prepared_statements(&self) -> Vec<Vec<Value>> {
        self.statements.map_or_else(Vec::new, |statements| {
            statements.iter().map(|(name, statement)| statement.row(name)).collect()
        })
    }

    fn current_of(&self, cursor: &str, table: u32) -> Result<(Value, Value)> {
        let portals = self.portals.map(RefCell::borrow);
        let portal = portals.as_ref().and_then(|portals| portals.find(cursor.as_bytes()));
        let Some(portal) = portal else {
            return Err(Error::new(
                SqlState::UNDEFINED_CURSOR,
                format!("cursor \"{cursor}\" does not exist"),
            ));
        };
        let relname = self.catalog.relation(table).map(|r| r.name.clone()).unwrap_or_default();
        let (oid, tid) = portal.current_of(cursor, table, &relname)?;
        // A version that the transaction updated after the fetch has a newer version, which the scan of `TidScan` finds.
        let latest = match (self.activity, &self.view, &oid, &tid) {
            (Some((_, store)), Some(view), Value::Oid(relation), Value::Int8(id)) => {
                let id = store.heap().latest_tid(view, *relation, id.cast_unsigned());
                Value::Int8(id.cast_signed())
            }
            _ => tid,
        };
        Ok((oid, latest))
    }

    fn notify(&self, channel: &str, payload: &str) -> Result<()> {
        let Some(conn) = self.conn else {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "NOTIFY in a session with no server is not supported yet",
            ));
        };
        conn.notify(channel, payload)
    }

    fn listening_channels(&self) -> Vec<String> {
        self.conn.map_or_else(Vec::new, Connection::listening_channels)
    }

    fn cursors(&self) -> Vec<Vec<Value>> {
        self.portals.map_or_else(Vec::new, |portals| {
            portals.borrow().iter().filter_map(|(name, portal)| portal.cursor_row(name)).collect()
        })
    }

    fn all_settings(&self) -> Vec<Vec<Value>> {
        let text = |text: Option<String>| text.map_or(Value::Null, Value::text);
        let settings = self.settings();
        settings
            .show_all_current()
            .map(|current| {
                let parameter = current.parameter;
                let (min, max) = parameter.range();
                let enumvals = parameter.enum_names().map_or(Value::Null, |names| {
                    let elements = names.into_iter().map(|name| Some(Value::text(name)));
                    Value::Array(Box::new(Array::one(elements.collect())))
                });
                vec![
                    Value::text(parameter.name),
                    Value::text(current.setting),
                    text(parameter.unit().map(str::to_owned)),
                    Value::text(parameter.category),
                    Value::text(parameter.short_desc),
                    text(parameter.extra_desc.map(str::to_owned)),
                    Value::text(parameter.context.name()),
                    Value::text(parameter.vartype()),
                    Value::text(current.source),
                    text(min),
                    text(max),
                    enumvals,
                    // The boot value of `server_version` is `PG_VERSION`, the version of the build, so it is the version of rupg.
                    text(if parameter.name == "server_version" {
                        Some(crate::connection::server_version())
                    } else {
                        parameter.boot_text()
                    }),
                    text(current.reset),
                    text(current.location.as_ref().map(|(file, _)| file.clone())),
                    current.location.map_or(Value::Null, |(_, line)| Value::Int4(line)),
                    Value::Bool(false),
                ]
            })
            .collect()
    }

    fn set_setting(&self, name: &str, value: Option<&str>, local: bool) -> Result<String> {
        self.settings().check_definer(name)?;
        if let Some(value) = value {
            utility::check_role(name, value, self.login, self.session_user)?;
        }
        let config = match value {
            Some(value) if utility::is_ts_config(name) => {
                Some(utility::check_ts_config(value, self)?)
            }
            _ => None,
        };
        let value = config.as_deref().or(value);
        let action = if local { Action::Local } else { Action::Set };
        let mut settings = self.settings.borrow_mut();
        settings.set(name, value, action, Origin::Statement)?;
        Ok(settings.show(name)?.1)
    }

    fn setting_list(&self, name: &str, text: &str) -> Option<Vec<String>> {
        guc::find(name).filter(|parameter| parameter.has(guc::flag::LIST_QUOTE))?;
        guc::split_identifiers(text, ',')
    }

    fn start_function_call(&self, config: &[String], definer: Option<u32>) -> Result<()> {
        let definer = definer.map(|role| (role, self.role_name(role)));
        self.settings.borrow_mut().push_function(config, definer)
    }

    fn end_function_call(&self) {
        self.settings.borrow_mut().pop_function();
    }
}

impl Env for Reader<'_> {
    fn check_function(&self, function: &rupg_catalog::Function) -> Result<()> {
        rupg_plpgsql::validate(self, function)
    }

    fn function_setting(&self, set: &VariableSetStmt) -> Result<(String, Option<String>)> {
        let name = set.name.as_deref().unwrap_or_default();
        let settings = self.settings();
        let value = match set.kind {
            VariableSetKind::VAR_SET_VALUE => utility::flatten(name, &set.args)?,
            VariableSetKind::VAR_SET_CURRENT => Some(settings.show(name)?.1),
            _ => None,
        };
        Ok((settings.check_stored(name, value.as_deref())?, value))
    }

    fn push_function_settings(&self, config: &[String]) -> Result<()> {
        self.settings.borrow_mut().push_function(config, None)
    }

    fn pop_function_settings(&self) {
        self.settings.borrow_mut().pop_function();
    }

    fn command_counter_increment(&self, catalog: &Catalog) {
        use_types(catalog);
    }

    fn search_path(&self) -> Vec<u32> {
        self.path().into_iter().map(|(_, oid)| oid).collect()
    }

    fn namespace(&self, name: &str) -> Option<u32> {
        self.schema(name)
    }

    fn temp_number(&self) -> Option<i32> {
        self.activity.and_then(|(activity, _)| activity.number())
    }

    fn database(&self) -> String {
        self.database.to_owned()
    }

    fn input(&self, ty: u32, text: &str, typmod: i32) -> Result<Value> {
        let value = rupg_func::input(ty, text, typmod, self)?;
        rupg_exec::check_input(ty, &value, self)?;
        Ok(value)
    }

    fn input_at(&self, ty: u32, text: &str, typmod: i32, at: Option<usize>) -> Result<Value> {
        let before = self.warnings.borrow().len();
        let value = self.input(ty, text, typmod);
        let mut warnings = self.warnings.borrow_mut();
        let new = warnings.split_off(before);
        warnings.extend(new.into_iter().map(|warning| warning.with_position(at)));
        value
    }

    fn catalog(&self) -> Option<&Catalog> {
        Some(&self.catalog)
    }

    fn transition_table(&self, name: &str) -> Option<u32> {
        let transition = self.transition.as_ref()?;
        transition.rows(name).map(|_| transition.rows.table)
    }

    fn allow_system_table_mods(&self) -> bool {
        self.settings().get("allow_system_table_mods").as_deref() == Some("on")
    }

    fn replica(&self) -> bool {
        self.settings().get("session_replication_role").as_deref() == Some("replica")
    }

    fn restrict_views(&self) -> bool {
        let setting = self.settings().get("restrict_nonsystem_relation_kind");
        setting.and_then(|s| guc::restricted_kinds(&s).ok()).is_some_and(|(view, _)| view)
    }

    fn check_function_bodies(&self) -> bool {
        self.settings().get("check_function_bodies").as_deref() != Some("off")
    }

    fn filling(&self) -> u32 {
        self.conn.map_or(0, |conn| conn.filling.get())
    }

    fn sequence_row(&self, file: u32) -> Option<SequenceData> {
        rupg_func::Session::sequence_row(self, file)
    }

    fn deparse(&self, text: &str, relation: u32) -> Result<String> {
        rupg_func::deparse_relation_expression(text, relation, self)
    }

    fn evaluate(&self, expr: &rupg_analyze::Expr) -> Result<Value> {
        rupg_exec::evaluate(expr, self)
    }

    fn compare(&self, ty: u32, a: &Value, b: &Value) -> Result<std::cmp::Ordering> {
        rupg_func::compare(ty, a, ty, b)
    }

    fn deparse_constant(&self, text: &str) -> Result<String> {
        rupg_func::deparse_constant(text, self)
    }
}

/// `parse_analyze` and the plan of a statement.
///
/// # Errors
///
/// The errors of the analysis, and `0A000` for a part of the statement that the engine cannot run yet.
pub(crate) fn plan(node: &Node, reader: &Reader<'_>, params: &Params) -> Result<Plan> {
    rupg_exec::prepare(rupg_analyze::analyze(node, reader, params)?, reader)
}

/// [`plan`] for a statement that the server runs for its own work, such as the action of a foreign key, with no checks of the privileges and no row security, as the queries of `ri_triggers.c` run as the owner of the table with `SECURITY_NOFORCE_RLS`.
///
/// # Errors
///
/// The errors of [`plan`].
pub(crate) fn plan_internal(node: &Node, reader: &Reader<'_>, params: &Params) -> Result<Plan> {
    let mut query = rupg_analyze::analyze(node, reader, params)?;
    // The plan keeps the columns of the privileges, which give the columns that the write changes.
    for perm in &mut query.perms {
        perm.required = 0;
    }
    rupg_exec::prepare(query, reader)
}

/// `_SPI_error_callback` for an error of the plan of the query of a function: an error with a position loses it, because the position is in the text of the query and not in the text of the client, and another error gets the text of the query in its context.
pub(crate) fn inner_error(error: Error, text: &str) -> Error {
    if error.position().is_some() {
        error.with_position(None)
    } else {
        error.with_context(format!("SQL statement \"{text}\""))
    }
}

/// The checks of `SPI_cursor_open` for the statement of a query of a function: `42P11` for a statement that gives no rows, and `0A000` for a statement that changes the database, because the plan is read-only. A `SELECT` passes.
fn cursor_error(node: &Node, text: &str) -> Option<Error> {
    let changes = |returning: &Option<Box<rupg_sql::nodes::ReturningClause>>| {
        returning.as_ref().is_some_and(|clause| !clause.exprs.is_empty())
    };
    let (tag, rows) = match node {
        Node::SelectStmt(stmt) if rupg_analyze::select_into(stmt).is_some() => {
            ("SELECT INTO", false)
        }
        Node::SelectStmt(_) => return None,
        Node::VariableSetStmt(stmt) => {
            let reset =
                matches!(stmt.kind, VariableSetKind::VAR_RESET | VariableSetKind::VAR_RESET_ALL);
            (if reset { "RESET" } else { "SET" }, false)
        }
        Node::VariableShowStmt(_) => ("SHOW", true),
        Node::ExplainStmt(_) => ("EXPLAIN", true),
        Node::InsertStmt(stmt) => ("INSERT", changes(&stmt.returningClause)),
        Node::UpdateStmt(stmt) => ("UPDATE", changes(&stmt.returningClause)),
        Node::DeleteStmt(stmt) => ("DELETE", changes(&stmt.returningClause)),
        Node::MergeStmt(stmt) => ("MERGE", changes(&stmt.returningClause)),
        _ => return Some(utility::not_supported(text)),
    };
    Some(if rows {
        Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("{tag} is not allowed in a non-volatile function"),
        )
        .with_context(format!("SQL statement \"{text}\""))
    } else {
        Error::new(
            SqlState::INVALID_CURSOR_DEFINITION,
            format!("cannot open {tag} query as cursor"),
        )
    })
}

/// True when the plan gives rows, which `RowDescription` describes: a query that reads, or a query that writes with `RETURNING`. A query that writes with no `RETURNING` gives `NoData`.
pub(crate) fn returns_rows(plan: &Plan) -> bool {
    plan.write().is_none_or(|write| write.returning.is_some())
}

/// True for a plan whose first column has the type `void`, as the plan of a `CALL` of a procedure with no output arguments has.
pub(crate) fn is_void(plan: &Plan) -> bool {
    plan.columns().first().is_some_and(|target| target.expr.ty == oid::VOID)
}

/// The columns of the result of a plan. A column of a domain has the base type and the typmod of the domain, as `SendRowDescriptionMessage` gives them.
pub(crate) fn columns(plan: &Plan) -> Vec<Column> {
    plan.columns()
        .iter()
        .map(|target| {
            let (ty, typmod) = rupg_analyze::types::base_typmod(target.expr.ty, target.expr.typmod);
            Column {
                name: target.name.clone(),
                ty,
                size: builtin::type_by_oid(ty).map_or(-1, |row| row.len),
                typmod,
                origin: target.origin.unwrap_or_default(),
            }
        })
        .collect()
}

/// Runs a plan and gives its rows in the result format of each column: 1 is binary and any other code is text. `Execute` checks the codes when it sends the first row.
///
/// # Errors
///
/// The errors of the functions of the plan and of the output functions.
pub(crate) fn run(
    plan: &Plan,
    params: &[Value],
    reader: &Reader<'_>,
    formats: &[i16],
) -> Result<Vec<Row>> {
    let types: Vec<u32> = plan.columns().iter().map(|target| target.expr.ty).collect();
    plan.run(params, reader)?.iter().map(|values| encode(&types, values, reader, formats)).collect()
}

/// [`run`] for a query that sends its rows to the client, as `PortalRunSelect` sends them. When the run fails, the error gives the rows that PostgreSQL sends before the error. See [`Plan::run_select`].
///
/// # Errors
///
/// The rows before the error, and the errors of the functions of the plan and of the output functions.
pub(crate) fn run_select(
    plan: &Plan,
    params: &[Value],
    reader: &Reader<'_>,
    formats: &[i16],
) -> std::result::Result<Vec<Row>, (Vec<Row>, Error)> {
    let types: Vec<u32> = plan.columns().iter().map(|target| target.expr.ty).collect();
    let (values, failed) = match plan.run_select(params, reader) {
        Ok(values) => (values, None),
        Err((values, error)) => (values, Some(error)),
    };
    let mut rows = Vec::with_capacity(values.len());
    for values in &values {
        // The output function of a row runs before the next row is made, so its error comes first.
        match encode(&types, values, reader, formats) {
            Ok(row) => rows.push(row),
            Err(error) => return Err((rows, error)),
        }
    }
    match failed {
        None => Ok(rows),
        Some(error) => Err((rows, error)),
    }
}

/// The bytes of the values of a row of the types `types` in the result format of each column: 1 is binary and any other code is text.
///
/// # Errors
///
/// The errors of the output functions.
pub(crate) fn encode(
    types: &[u32],
    values: &[Value],
    reader: &Reader<'_>,
    formats: &[i16],
) -> Result<Row> {
    let mut row = Vec::with_capacity(values.len());
    for (i, (&ty, value)) in types.iter().zip(values).enumerate() {
        if matches!(value, Value::Null) {
            row.push(None);
            continue;
        }
        let mut bytes = Vec::new();
        if formats.get(i) == Some(&1) {
            rupg_func::send(ty, value, &mut bytes)?;
        } else {
            rupg_func::output(ty, value, reader, &mut bytes)?;
        }
        row.push(Some(bytes));
    }
    Ok(row)
}

/// The value of parameter `number`, from 1, of a `Bind`: the input function of the type for the text format and the receive function for the binary format.
///
/// # Errors
///
/// `22023` for a format code that is not 0 or 1, and the error of the type function for a bad value.
pub(crate) fn param(
    ty: u32,
    format: i16,
    value: Option<&[u8]>,
    number: usize,
    reader: &Reader<'_>,
) -> Result<Value> {
    match format {
        0 | 1 if value.is_none() => Ok(Value::Null),
        0 => {
            let text = Recv::new(value.unwrap_or_default()).text().map_err(type_error)?;
            rupg_func::input(ty, text, -1, reader)
        }
        1 => rupg_func::receive(ty, value.unwrap_or_default(), -1, number, reader),
        _ => Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("unsupported format code: {format}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zones_of_the_canonical_names() {
        let at = |text: &str, instant: i64| {
            let zone = session_zone(text).unwrap();
            let (offset, abbrev) = zone.at(instant);
            (offset, abbrev.to_owned())
        };
        assert_eq!(at("UTC", 0), (0, "UTC".to_owned()));
        assert_eq!(at("Etc/GMT+5", 0), (-18000, "-05".to_owned()));
        assert_eq!(at("<+02>-02", 0), (7200, "+02".to_owned()));
        // 2024-07-01 12:00 UTC and 2024-01-01 12:00 UTC.
        assert_eq!(at("Europe/Paris", 1_719_835_200), (7200, "CEST".to_owned()));
        assert_eq!(at("Europe/Paris", 1_704_110_400), (3600, "CET".to_owned()));
        assert!(session_zone("Nowhere/X").is_err());
    }

    #[test]
    fn date_styles() {
        assert_eq!(date_format("ISO, MDY"), DateFormat::ISO_MDY);
        let german = date_format("German, DMY");
        assert_eq!((german.style, german.order), (DateStyle::German, DateOrder::Dmy));
    }
}
