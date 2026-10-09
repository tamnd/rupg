//! The row of the session in the table of the sessions, which `pg_stat_activity` shows: what `pgstat_bestart`, `pgstat_report_activity`, `pgstat_report_xact_timestamp` and `pgstat_report_appname` write, and the snapshot of the table that a transaction reads.
//!
//! The session reports the state `active` and the text of the statement when a `Query`, `Parse`, `Bind` or `Execute` starts, and the idle state that `ReadyForQuery` gives before it sends the message. The start of a transaction sets `xact_start`, and the end of a transaction clears it and drops the snapshot. When `track_activities` is off, the next report sets the state `disabled` and clears the text and the times, as in PostgreSQL.

use std::cell::RefCell;
use std::sync::Arc;

use rupg_func::{Backend, BackendState, Client};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_wire::TransactionStatus;

use super::Connection;
use crate::guc::{self, Setting, Settings};
use crate::query;
use crate::store::Store;

/// `NAMEDATALEN - 1`, the most bytes of `application_name` in the table.
const APPLICATION_NAME_BYTES: usize = 63;

/// What a session knows of the table of the sessions.
#[derive(Debug, Default)]
pub(crate) struct Activity {
    /// The number of the session in the table, after [`Connection::greet`].
    number: Option<i32>,
    /// The two ends of the connection.
    pub(crate) client: Client,
    pub(crate) server: Client,
    /// The snapshot of the table that the transaction reads, after its first read.
    snapshot: RefCell<Option<Arc<[Backend]>>>,
}

impl Activity {
    /// The snapshot of the transaction. The first read takes it, after it writes the current `application_name` of the session, which PostgreSQL writes when the setting changes.
    pub(crate) fn backends(&self, store: &Store, settings: &Settings) -> Arc<[Backend]> {
        let mut snapshot = self.snapshot.borrow_mut();
        if let Some(rows) = &*snapshot {
            return Arc::clone(rows);
        }
        if let Some(number) = self.number {
            let application = application_name(settings);
            store.report(number, |backend| backend.application = application);
        }
        let rows = store.backends();
        *snapshot = Some(Arc::clone(&rows));
        rows
    }

    /// `pgstat_clear_backend_activity_snapshot`.
    pub(crate) fn clear(&self) {
        self.snapshot.borrow_mut().take();
    }
}

/// The value of `application_name`, cut to the bytes that the table keeps.
fn application_name(settings: &Settings) -> String {
    let mut name = settings.get("application_name").unwrap_or_default();
    clip(&mut name, APPLICATION_NAME_BYTES);
    name
}

/// `pg_mbcliplen`: cuts the text to at most `max` bytes at the end of a character.
fn clip(text: &mut String, max: usize) {
    if text.len() > max {
        let end = (0..=max).rev().find(|&at| text.is_char_boundary(at)).unwrap_or(0);
        text.truncate(end);
    }
}

/// `pgstat_track_activities`.
fn tracked(settings: &Settings) -> bool {
    settings.get("track_activities").as_deref() != Some("off")
}

/// `pgstat_track_activity_query_size - 1`, the most bytes of the text of a statement in the table.
fn query_bytes(settings: &Settings) -> usize {
    let size = match guc::find("track_activity_query_size").map(|p| settings.setting(p)) {
        Some(Setting::Int(size)) => size,
        _ => 1024,
    };
    usize::try_from(size).map_or(0, |size| size.saturating_sub(1))
}

impl Connection {
    /// Sets the two ends of the connection, the client and the server, before [`Connection::greet`]. A connection with no addresses shows null addresses.
    pub fn set_addresses(&mut self, client: Client, server: Client) {
        self.activity.client = client;
        self.activity.server = server;
    }

    /// `pgstat_bestart`: puts the session in the table of its store.
    pub(super) fn join(&mut self) {
        let database = builtin::named(Named::Database)
            .iter()
            .find(|row| row.name == self.database)
            .map_or(0, |row| row.oid);
        let backend = Backend {
            pid: self.pid,
            database,
            role: self.role,
            application: application_name(self.settings.get_mut()),
            backend_start: query::now(&*self.clock),
            client: self.activity.client,
            ..Backend::default()
        };
        self.activity.number = Some(self.store.join(backend));
    }

    /// `pgstat_report_activity`: the state of the session, and the text of the statement that starts when `text` is `Some`.
    pub(super) fn report(&mut self, state: BackendState, text: Option<&str>) {
        let Some(number) = self.activity.number else { return };
        let settings = self.settings.get_mut();
        let application = application_name(settings);
        let tracked = tracked(settings);
        let text = text.map(|text| {
            let mut text = text.to_owned();
            clip(&mut text, query_bytes(settings));
            text
        });
        let now = query::now(&*self.clock);
        let start = self.statement_start;
        self.store.report(number, |backend| {
            backend.application = application;
            backend.reading = state != BackendState::Running;
            if !tracked {
                if backend.state != BackendState::Disabled {
                    backend.state = BackendState::Disabled;
                    backend.state_change = None;
                    backend.query.clear();
                    backend.query_start = None;
                    backend.transaction_start = None;
                }
                return;
            }
            backend.state = state;
            backend.state_change = Some(now);
            if let Some(text) = text {
                backend.query = text;
                backend.query_start = Some(start);
            }
        });
    }

    /// The idle state that `ReadyForQuery` reports.
    pub(super) fn report_idle(&mut self) {
        let state = match self.transaction.status() {
            TransactionStatus::Idle => BackendState::Idle,
            TransactionStatus::Block => BackendState::IdleInTransaction,
            TransactionStatus::Failed => BackendState::IdleInTransactionAborted,
        };
        self.report(state, None);
    }

    /// `pgstat_report_xact_timestamp`: the start of the transaction, or `None` at its end.
    fn report_transaction(&mut self, start: Option<i64>) {
        let Some(number) = self.activity.number else { return };
        if tracked(self.settings.get_mut()) {
            self.store.report(number, |backend| backend.transaction_start = start);
        }
    }

    /// The start of a transaction.
    pub(super) fn transaction_started(&mut self) {
        self.report_transaction(Some(self.transaction_start));
    }

    /// The end of a transaction, at a commit or at an abort: `AtEOXact_PgStat` drops the snapshot of the table.
    pub(super) fn transaction_ended(&mut self) {
        self.activity.clear();
        self.report_transaction(None);
    }

    /// The end of the session, which frees its row of the table.
    pub(super) fn leave(&mut self) {
        if let Some(number) = self.activity.number.take() {
            self.store.leave(number);
        }
    }
}
