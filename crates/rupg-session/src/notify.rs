//! `LISTEN`, `UNLISTEN` and `NOTIFY`, as `async.c` runs them. A transaction keeps its actions and its notifications until it ends. At a commit the actions change the channels of the session, then each notification goes to the queue of each session of the database that listens. A session sends the notifications of its queue on the channels that it listens on before its next `ReadyForQuery` outside of a transaction block.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rupg_common::{Error, Result, SqlState};

/// `NAMEDATALEN`: a channel name has fewer bytes.
const NAME_LIMIT: usize = 64;
/// `NOTIFY_PAYLOAD_MAX_LENGTH`: a payload has fewer bytes.
const PAYLOAD_LIMIT: usize = 8000;

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

/// A session that listens: its database, its channels and the notifications that it did not send yet.
#[derive(Debug)]
struct Listener {
    database: u32,
    channels: BTreeSet<String>,
    queue: VecDeque<Notification>,
}

/// The listeners of a server, by the process ID of the session.
#[derive(Debug, Default)]
pub(crate) struct Listeners {
    table: Mutex<BTreeMap<i32, Listener>>,
}

impl Listeners {
    fn table(&self) -> MutexGuard<'_, BTreeMap<i32, Listener>> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `AtCommit_Notify` and `SignalBackends`: the actions of the transaction of the session `pid` of the database `database` change its channels, then each notification goes to each session of the database that listens, the session itself too.
    pub(crate) fn commit(&self, pid: i32, database: u32, pending: Pending) {
        let mut table = self.table();
        for action in pending.actions {
            match action {
                Action::Listen(channel) => {
                    table
                        .entry(pid)
                        .or_insert_with(|| Listener {
                            database,
                            channels: BTreeSet::new(),
                            queue: VecDeque::new(),
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
        for (channel, payload) in pending.notifies {
            for listener in table.values_mut().filter(|l| l.database == database) {
                if listener.channels.contains(&channel) {
                    listener.queue.push_back(Notification {
                        pid,
                        channel: channel.clone(),
                        payload: payload.clone(),
                    });
                }
            }
        }
    }

    /// `asyncQueueReadAllNotifications`: takes the notifications of the queue of the session `pid` on the channels that it listens on now.
    pub(crate) fn take(&self, pid: i32) -> Vec<Notification> {
        let mut table = self.table();
        let Some(listener) = table.get_mut(&pid) else { return Vec::new() };
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
        self.table().get(&pid).map(|l| l.channels.iter().cloned().collect()).unwrap_or_default()
    }

    /// `Async_UnlistenOnExit`: the session `pid` ends.
    pub(crate) fn leave(&self, pid: i32) {
        self.table().remove(&pid);
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
}
