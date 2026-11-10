//! `LISTEN`, `UNLISTEN` and `NOTIFY`, as `async.c` runs them. A transaction keeps its actions and its notifications until it ends. At a commit the actions change the channels of the session, then each notification goes to the queue of each session of the database that listens. A session sends the notifications of its queue on the channels that it listens on before its next `ReadyForQuery` outside of a transaction block.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rupg_common::{Error, Result, SqlState};

/// `NAMEDATALEN`: a channel name has fewer bytes.
const NAME_LIMIT: usize = 64;
/// `NOTIFY_PAYLOAD_MAX_LENGTH`: a payload has fewer bytes.
const PAYLOAD_LIMIT: usize = 8000;
/// `BLCKSZ`: the bytes of a page of the queue.
const PAGE: u64 = 8192;
/// `AsyncQueueEntryEmptySize`: the bytes of an entry of the queue with an empty channel name and an empty payload, the header and two terminating zeros.
const ENTRY: u64 = 18;
/// The default of `max_notify_queue_pages`: the pages that the queue can have.
const QUEUE_PAGES: u64 = 1_048_576;

/// A notification: the process ID of the session that sent it, the channel and the payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notification {
    pub(crate) pid: i32,
    pub(crate) channel: String,
    pub(crate) payload: String,
}

/// An action of `pendingActions`.
#[derive(Clone, Debug)]
enum Action {
    Listen(String),
    Unlisten(String),
    UnlistenAll,
}

/// The actions and the notifications of a transaction, `pendingActions` and `pendingNotifies`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Pending {
    actions: Vec<Action>,
    /// The channels and the payloads, with no two the same.
    notifies: Vec<(String, String)>,
}

impl Pending {
    /// `Async_Notify`: adds a notification, unless the transaction has the same one.
    ///
    /// # Errors
    ///
    /// `22023` for an empty channel name, a channel name of 64 bytes or more, or a payload of 8000 bytes or more.
    pub(crate) fn notify(&mut self, channel: &str, payload: &str) -> Result<()> {
        let invalid = |message| Err(Error::new(SqlState::INVALID_PARAMETER_VALUE, message));
        if channel.is_empty() {
            return invalid("channel name cannot be empty");
        }
        if channel.len() >= NAME_LIMIT {
            return invalid("channel name too long");
        }
        if payload.len() >= PAYLOAD_LIMIT {
            return invalid("payload string too long");
        }
        if !self.notifies.iter().any(|(c, p)| c == channel && p == payload) {
            self.notifies.push((channel.to_string(), payload.to_string()));
        }
        Ok(())
    }

    /// `Async_Listen`.
    pub(crate) fn listen(&mut self, channel: &str) {
        self.actions.push(Action::Listen(channel.to_string()));
    }

    /// `Async_Unlisten`, or `Async_UnlistenAll` for `None`.
    pub(crate) fn unlisten(&mut self, channel: Option<&str>) {
        self.actions.push(channel.map_or(Action::UnlistenAll, |c| Action::Unlisten(c.to_string())));
    }

    /// True when the transaction has no actions and no notifications.
    pub(crate) fn is_empty(&self) -> bool {
        self.actions.is_empty() && self.notifies.is_empty()
    }
}

/// A session that listens: its database, its channels, the notifications that it did not send yet, and the place in the queue of the first of them, as `QUEUE_BACKEND_POS` keeps the place that the session read up to.
#[derive(Debug)]
struct Listener {
    database: u32,
    channels: BTreeSet<String>,
    queue: VecDeque<Notification>,
    from: Option<u64>,
}

/// The listeners of a server, by the process ID of the session, and `QUEUE_HEAD`: the place in bytes after the last entry of the queue.
#[derive(Debug, Default)]
pub(crate) struct Listeners {
    table: Mutex<(BTreeMap<i32, Listener>, u64)>,
    /// `blks_zeroed` of the SLRU `notify`: the pages of the queue that the entries started.
    zeroed: AtomicI64,
}

impl Listeners {
    fn table(&self) -> MutexGuard<'_, (BTreeMap<i32, Listener>, u64)> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `blks_zeroed` of the SLRU `notify`.
    pub(crate) fn zeroed(&self) -> i64 {
        self.zeroed.load(Ordering::Relaxed)
    }

    /// `asyncQueueUsage`: the part of the pages of the queue that the notifications use, from the page of `QUEUE_TAIL`, the first notification that a listener did not read, to the page of `QUEUE_HEAD`.
    pub(crate) fn usage(&self) -> f64 {
        let guard = self.table();
        let (table, head) = &*guard;
        let tail = table.values().filter_map(|l| l.from).min().unwrap_or(*head);
        let occupied = head / PAGE - tail / PAGE;
        #[expect(clippy::cast_precision_loss, reason = "the pages of the queue fit in 53 bits")]
        let usage = occupied as f64 / QUEUE_PAGES as f64;
        usage
    }

    /// `AtCommit_Notify` and `SignalBackends`: the actions of the transaction of the session `pid` of the database `database` change its channels, then each notification goes to each session of the database that listens, the session itself too.
    pub(crate) fn commit(&self, pid: i32, database: u32, pending: Pending) {
        let mut guard = self.table();
        let (table, head) = &mut *guard;
        for action in pending.actions {
            match action {
                Action::Listen(channel) => {
                    table
                        .entry(pid)
                        .or_insert_with(|| Listener {
                            database,
                            channels: BTreeSet::new(),
                            queue: VecDeque::new(),
                            from: None,
                        })
                        .channels
                        .insert(channel);
                }
                Action::Unlisten(channel) => {
                    if let Some(listener) = table.get_mut(&pid) {
                        listener.channels.remove(&channel);
                    }
                }
                Action::UnlistenAll => {
                    if let Some(listener) = table.get_mut(&pid) {
                        listener.channels.clear();
                    }
                }
            }
        }
        if table.get(&pid).is_some_and(|l| l.channels.is_empty() && l.queue.is_empty()) {
            table.remove(&pid);
        }
        // `asyncQueueAddEntries` zeroes the first page of the queue, and then each page that the head moves to.
        let first = u64::from(*head == 0 && !pending.notifies.is_empty());
        let start_page = *head / PAGE;
        for (channel, payload) in pending.notifies {
            // `asyncQueueAddEntries`: an entry has its length aligned to 8 bytes, and an entry that does not fit in the rest of a page goes at the start of the next page.
            let size = (ENTRY + (channel.len() + payload.len()) as u64).next_multiple_of(8);
            if *head % PAGE + size > PAGE {
                *head = head.next_multiple_of(PAGE);
            }
            let at = *head;
            *head += size;
            for listener in table.values_mut().filter(|l| l.database == database) {
                if listener.channels.contains(&channel) {
                    listener.from.get_or_insert(at);
                    listener.queue.push_back(Notification {
                        pid,
                        channel: channel.clone(),
                        payload: payload.clone(),
                    });
                }
            }
        }
        let pages = first + *head / PAGE - start_page;
        if pages > 0 {
            self.zeroed.fetch_add(i64::try_from(pages).unwrap_or(i64::MAX), Ordering::Relaxed);
        }
    }

    /// `asyncQueueReadAllNotifications`: takes the notifications of the queue of the session `pid` on the channels that it listens on now.
    pub(crate) fn take(&self, pid: i32) -> Vec<Notification> {
        let mut guard = self.table();
        let table = &mut guard.0;
        let Some(listener) = table.get_mut(&pid) else { return Vec::new() };
        listener.from = None;
        let queue = std::mem::take(&mut listener.queue);
        let notifications =
            queue.into_iter().filter(|n| listener.channels.contains(&n.channel)).collect();
        if listener.channels.is_empty() {
            table.remove(&pid);
        }
        notifications
    }

    /// `pg_listening_channels`: the channels that the session `pid` listens on.
    pub(crate) fn channels(&self, pid: i32) -> Vec<String> {
        self.table().0.get(&pid).map(|l| l.channels.iter().cloned().collect()).unwrap_or_default()
    }

    /// `Async_UnlistenOnExit`: the session `pid` ends.
    pub(crate) fn leave(&self, pid: i32) {
        self.table().0.remove(&pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_sends_to_the_listeners_of_the_database() {
        let listeners = Listeners::default();
        let mut pending = Pending::default();
        pending.listen("a");
        listeners.commit(1, 5, pending);
        let mut pending = Pending::default();
        pending.listen("a");
        listeners.commit(2, 6, pending);
        let mut pending = Pending::default();
        pending.notify("a", "x").unwrap();
        pending.notify("a", "x").unwrap();
        pending.notify("b", "y").unwrap();
        listeners.commit(3, 5, pending);
        let sent = Notification { pid: 3, channel: "a".into(), payload: "x".into() };
        assert_eq!(listeners.take(1), [sent]);
        assert!(listeners.take(2).is_empty());
        assert!(listeners.take(1).is_empty());
        assert_eq!(listeners.channels(1), ["a"]);
        let mut pending = Pending::default();
        pending.unlisten(None);
        listeners.commit(1, 5, pending);
        assert!(listeners.channels(1).is_empty());
        listeners.leave(2);
        assert!(listeners.channels(2).is_empty());
    }

    #[test]
    fn the_usage_counts_the_pages_that_a_listener_did_not_read() {
        let listeners = Listeners::default();
        let mut pending = Pending::default();
        pending.listen("c1");
        listeners.commit(1, 5, pending);
        let mut pending = Pending::default();
        for n in 0..1000 {
            pending.notify("c1", &n.to_string()).unwrap();
        }
        listeners.commit(2, 5, pending);
        assert!(listeners.usage() > 0.0);
        assert_eq!(listeners.take(1).len(), 1000);
        assert!(listeners.usage() == 0.0);
    }
}
