//! `LISTEN`, `UNLISTEN` and `NOTIFY` of a session, as `standard_ProcessUtility` runs them, and the notifications that the session sends to its client.

use rupg_common::Error;
use rupg_sql::nodes::Node;
use rupg_wire::{CommandTag, OutBuf, TransactionStatus};

use super::{Connection, Done};

impl Connection {
    /// `standard_ProcessUtility` for `NOTIFY`, `LISTEN` and `UNLISTEN`, which `Async_Notify`, `Async_Listen`, `Async_Unlisten` and `Async_UnlistenAll` add to the transaction. `None` for another statement.
    pub(super) fn async_statement(&self, node: &Node) -> Option<Result<Done, Error>> {
        let mut pending = self.notify.borrow_mut();
        Some(match node {
            Node::NotifyStmt(stmt) => pending
                .notify(
                    stmt.conditionname.as_deref().unwrap_or_default(),
                    stmt.payload.as_deref().unwrap_or_default(),
                )
                .map(|()| Done::Tag(CommandTag::Notify)),
            Node::ListenStmt(stmt) => {
                pending.listen(stmt.conditionname.as_deref().unwrap_or_default());
                Ok(Done::Tag(CommandTag::Listen))
            }
            Node::UnlistenStmt(stmt) => {
                pending.unlisten(stmt.conditionname.as_deref());
                Ok(Done::Tag(CommandTag::Unlisten))
            }
            _ => return None,
        })
    }

    /// `pg_notify`: `Async_Notify` of a function.
    ///
    /// # Errors
    ///
    /// The errors of `Async_Notify` for the channel and the payload.
    pub(crate) fn notify(&self, channel: &str, payload: &str) -> Result<(), Error> {
        self.notify.borrow_mut().notify(channel, payload)
    }

    /// `pg_listening_channels`: the channels that the session listens on after its last commit.
    pub(crate) fn listening_channels(&self) -> Vec<String> {
        self.store.listeners().channels(self.pid)
    }

    /// `AtCommit_Notify` and `AtAbort_Notify`: at a commit the actions and the notifications of the transaction take effect, and at a rollback they go.
    pub(super) fn end_notify(&self, commit: bool) {
        let pending = self.notify.take();
        if commit && !pending.is_empty() {
            self.store.listeners().commit(self.pid, self.database_oid, pending);
        }
    }

    /// `ProcessNotifyInterrupt` before `ReadyForQuery`: outside of a transaction the session sends the notifications of its queue.
    pub(super) fn send_notifications(&self, out: &mut OutBuf) {
        if self.transaction.status() != TransactionStatus::Idle {
            return;
        }
        for sent in self.store.listeners().take(self.pid) {
            out.notification_response(sent.pid, sent.channel.as_bytes(), sent.payload.as_bytes());
        }
    }
}
