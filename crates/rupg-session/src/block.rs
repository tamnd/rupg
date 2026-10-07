//! The transaction block of a session: the values of `blockState` in `xact.c`, without the states of subtransactions and of parallel workers.
//!
//! The session has no transaction of the engine yet. The block decides what the client can see: the status byte of `ReadyForQuery`, the warnings and the tags of `BEGIN`, `COMMIT` and `ROLLBACK`, the error `25P02` in a failed block, and the end of each transaction, where the settings keep or restore the values that `SET` changed. Each method has the name of the function of `xact.c` that it follows.

use rupg_common::{Error, SqlState};
use rupg_wire::TransactionStatus;

/// One value of `blockState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Block {
    /// `TBLOCK_DEFAULT`: no transaction.
    Default,
    /// `TBLOCK_STARTED`: the transaction of one statement.
    Started,
    /// `TBLOCK_BEGIN`: the current statement ran `BEGIN`.
    Begin,
    /// `TBLOCK_INPROGRESS`: a transaction block that the client started.
    InProgress,
    /// `TBLOCK_IMPLICIT_INPROGRESS`: the block of a `Query` with more than one statement.
    Implicit,
    /// `TBLOCK_END`: the current statement ran `COMMIT`.
    End,
    /// `TBLOCK_ABORT`: a failed transaction block.
    Abort,
    /// `TBLOCK_ABORT_END`: the current statement ended a failed block.
    AbortEnd,
    /// `TBLOCK_ABORT_PENDING`: the current statement ran `ROLLBACK` in a block that did not fail.
    AbortPending,
}

/// What happens to the transaction at the end of a statement or after an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The transaction goes on, or there is none.
    None,
    /// The transaction commits.
    Commit,
    /// The transaction rolls back.
    Rollback,
}

/// The transaction block and the `AND CHAIN` flag of the statement that ends it.
#[derive(Clone, Copy, Debug)]
pub struct Transaction {
    block: Block,
    chain: bool,
}

impl Default for Transaction {
    fn default() -> Transaction {
        Transaction { block: Block::Default, chain: false }
    }
}

/// `%s can only be used in transaction blocks`, the error of `AND CHAIN` outside of a block.
fn only_in_blocks(statement: &str) -> Error {
    Error::new(
        SqlState::NO_ACTIVE_SQL_TRANSACTION,
        format!("{statement} can only be used in transaction blocks"),
    )
}

/// The warning of `COMMIT` and `ROLLBACK` outside of a block.
fn no_transaction() -> Error {
    Error::new(SqlState::NO_ACTIVE_SQL_TRANSACTION, "there is no transaction in progress")
}

impl Transaction {
    /// The current state.
    pub fn block(&self) -> Block {
        self.block
    }

    /// The status byte of `ReadyForQuery`.
    pub fn status(&self) -> TransactionStatus {
        match self.block {
            Block::Default | Block::Started => TransactionStatus::Idle,
            Block::Abort | Block::AbortEnd => TransactionStatus::Failed,
            _ => TransactionStatus::Block,
        }
    }

    /// `IsTransactionBlock`: true in a block that the client started and in an implicit block.
    pub fn in_block(&self) -> bool {
        !matches!(self.block, Block::Default | Block::Started)
    }

    /// True in a failed block, where only `COMMIT` and `ROLLBACK` run.
    pub fn failed(&self) -> bool {
        self.block == Block::Abort
    }

    /// `StartTransactionCommand`: a statement starts, and starts a transaction if there is none. It gives true when a transaction starts.
    pub fn start_command(&mut self) -> bool {
        let starts = self.block == Block::Default;
        if starts {
            self.block = Block::Started;
        }
        starts
    }

    /// `BeginImplicitTransactionBlock`: a statement of a `Query` with more than one statement starts.
    pub fn begin_implicit(&mut self) {
        if self.block == Block::Started {
            self.block = Block::Implicit;
        }
    }

    /// `EndImplicitTransactionBlock`: the last statement of a `Query` ends.
    pub fn end_implicit(&mut self) {
        if self.block == Block::Implicit {
            self.block = Block::Started;
        }
    }

    /// `BeginTransactionBlock`: `BEGIN` and `START TRANSACTION`. It gives the warning of a block that is already open.
    pub fn begin(&mut self) -> Option<Error> {
        match self.block {
            Block::Started | Block::Implicit => {
                self.block = Block::Begin;
                None
            }
            _ => Some(Error::new(
                SqlState::ACTIVE_SQL_TRANSACTION,
                "there is already a transaction in progress",
            )),
        }
    }

    /// `EndTransactionBlock`: `COMMIT` and `END`. It gives false when the statement rolls back a failed block, so the tag is `ROLLBACK`, and the warning of a `COMMIT` outside of a block.
    ///
    /// # Errors
    ///
    /// `COMMIT AND CHAIN` outside of a block.
    pub fn commit(&mut self, chain: bool) -> Result<(bool, Option<Error>), Error> {
        let mut warning = None;
        let commits = match self.block {
            Block::InProgress => {
                self.block = Block::End;
                true
            }
            Block::Implicit | Block::Started => {
                if chain {
                    return Err(only_in_blocks("COMMIT AND CHAIN"));
                }
                warning = Some(no_transaction());
                if self.block == Block::Implicit {
                    self.block = Block::End;
                }
                true
            }
            Block::Abort => {
                self.block = Block::AbortEnd;
                false
            }
            _ => true,
        };
        self.chain = chain;
        Ok((commits, warning))
    }

    /// `UserAbortTransactionBlock`: `ROLLBACK` and `ABORT`. It gives the warning of a `ROLLBACK` outside of a block.
    ///
    /// # Errors
    ///
    /// `ROLLBACK AND CHAIN` outside of a block.
    pub fn rollback(&mut self, chain: bool) -> Result<Option<Error>, Error> {
        let mut warning = None;
        match self.block {
            Block::InProgress => self.block = Block::AbortPending,
            Block::Abort => self.block = Block::AbortEnd,
            Block::Implicit | Block::Started => {
                if chain {
                    return Err(only_in_blocks("ROLLBACK AND CHAIN"));
                }
                warning = Some(no_transaction());
                self.block = Block::AbortPending;
            }
            _ => {}
        }
        self.chain = chain;
        Ok(warning)
    }

    /// `CommitTransactionCommand`: the end of a statement. It gives what happens to the transaction, and true when `AND CHAIN` starts a new block at once.
    pub fn finish_command(&mut self) -> (Ending, bool) {
        let chain = std::mem::take(&mut self.chain);
        let (ending, next) = match self.block {
            Block::Started | Block::End => (Ending::Commit, Block::Default),
            Block::Begin => (Ending::None, Block::InProgress),
            Block::AbortPending => (Ending::Rollback, Block::Default),
            // The failed block rolled back when it failed.
            Block::AbortEnd => (Ending::None, Block::Default),
            block => (Ending::None, block),
        };
        let chained = chain && next == Block::Default;
        self.block = if chained { Block::InProgress } else { next };
        (ending, chained)
    }

    /// `AbortCurrentTransaction`: the end of a statement that failed. A block that the client started becomes a failed block, and any other transaction ends.
    pub fn abort_current(&mut self) -> Ending {
        self.chain = false;
        let (ending, next) = match self.block {
            Block::Default | Block::Abort => (Ending::None, self.block),
            Block::InProgress => (Ending::Rollback, Block::Abort),
            Block::AbortEnd => (Ending::None, Block::Default),
            _ => (Ending::Rollback, Block::Default),
        };
        self.block = next;
        ending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs one statement of a `Query` of one statement, as `exec_simple_query` does.
    fn statement(t: &mut Transaction, run: impl FnOnce(&mut Transaction)) -> (Ending, bool) {
        t.start_command();
        run(t);
        t.finish_command()
    }

    #[test]
    fn a_block_commits() {
        let mut t = Transaction::default();
        assert_eq!(statement(&mut t, |t| assert!(t.begin().is_none())), (Ending::None, false));
        assert_eq!(t.status(), TransactionStatus::Block);
        assert!(t.begin().is_some());
        let commit = statement(&mut t, |t| assert_eq!(t.commit(false).unwrap(), (true, None)));
        assert_eq!(commit, (Ending::Commit, false));
        assert_eq!(t.status(), TransactionStatus::Idle);
    }

    #[test]
    fn a_failed_block_rolls_back_on_commit() {
        let mut t = Transaction::default();
        statement(&mut t, |t| assert!(t.begin().is_none()));
        t.start_command();
        assert_eq!(t.abort_current(), Ending::Rollback);
        assert!(t.failed());
        assert_eq!(t.status(), TransactionStatus::Failed);
        let commit = statement(&mut t, |t| assert_eq!(t.commit(false).unwrap(), (false, None)));
        assert_eq!(commit, (Ending::None, false));
        assert_eq!(t.status(), TransactionStatus::Idle);
    }

    #[test]
    fn outside_of_a_block() {
        let mut t = Transaction::default();
        let (commits, warning) = t.clone_run(|t| t.commit(false).unwrap());
        assert!(commits);
        assert_eq!(warning.unwrap().state(), SqlState::NO_ACTIVE_SQL_TRANSACTION);
        t.start_command();
        assert_eq!(
            t.commit(true).unwrap_err().message(),
            "COMMIT AND CHAIN can only be used in transaction blocks"
        );
        assert_eq!(t.abort_current(), Ending::Rollback);
        let rollback = statement(&mut t, |t| assert!(t.rollback(false).unwrap().is_some()));
        assert_eq!(rollback, (Ending::Rollback, false));
    }

    #[test]
    fn chain_opens_a_new_block() {
        let mut t = Transaction::default();
        statement(&mut t, |t| assert!(t.begin().is_none()));
        let commit = statement(&mut t, |t| assert!(t.commit(true).unwrap().0));
        assert_eq!(commit, (Ending::Commit, true));
        assert_eq!(t.block(), Block::InProgress);
        let rollback = statement(&mut t, |t| assert!(t.rollback(true).unwrap().is_none()));
        assert_eq!(rollback, (Ending::Rollback, true));
        assert_eq!(t.block(), Block::InProgress);
    }

    #[test]
    fn an_implicit_block() {
        let mut t = Transaction::default();
        t.start_command();
        t.begin_implicit();
        assert!(t.in_block());
        assert_eq!(t.finish_command(), (Ending::None, false));
        // BEGIN in an implicit block turns it into a block of the client.
        t.start_command();
        t.begin_implicit();
        assert!(t.begin().is_none());
        assert_eq!(t.finish_command(), (Ending::None, false));
        assert_eq!(t.block(), Block::InProgress);
    }

    impl Transaction {
        /// Runs `run` on a statement and ends it, and gives what `run` gave.
        fn clone_run<T>(&mut self, run: impl FnOnce(&mut Transaction) -> T) -> T {
            self.start_command();
            let out = run(self);
            self.finish_command();
            out
        }
    }
}
