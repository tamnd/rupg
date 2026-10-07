//! Row id blocks (spec/10 section 10.2.1).
//!
//! Each table and shard gives row ids from one counter. A worker takes a block of 1024 ids and uses them without a shared write, so workers do not write the same cache line for each insert. The ids of a block that a worker does not use are a gap, and gaps are normal, because row ids need not be dense.

use std::sync::atomic::{AtomicU64, Ordering};

use rupg_common::{Error, Result, RowId, ShardId, SqlState};

/// The row id counter of a table and shard.
#[derive(Debug)]
pub struct RowIds {
    shard: ShardId,
    next: AtomicU64,
}

impl RowIds {
    /// The number of ids in a block.
    pub const BLOCK: u64 = 1024;

    /// A counter that gives local numbers from `next`.
    pub fn new(shard: ShardId, next: u64) -> RowIds {
        RowIds { shard, next: AtomicU64::new(next) }
    }

    /// The first local number that no block holds. The catalog records it, and the log records each change, once for each block.
    pub fn next_local(&self) -> u64 {
        self.next.load(Ordering::Acquire)
    }

    /// Takes the next block. The last block can have fewer ids. When no ids are left, the result is SQLSTATE `54000`.
    pub fn take(&self) -> Result<RowIdBlock> {
        let mut start = self.next.load(Ordering::Acquire);
        loop {
            if start > RowId::MAX_LOCAL {
                return Err(Error::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!("the row ids of shard {} are used up", self.shard),
                ));
            }
            let end = start.saturating_add(RowIds::BLOCK).min(RowId::MAX_LOCAL + 1);
            match self.next.compare_exchange_weak(start, end, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(RowIdBlock { shard: self.shard, next: start, end }),
                Err(now) => start = now,
            }
        }
    }
}

/// A block of row ids that one writer owns.
#[derive(Debug)]
pub struct RowIdBlock {
    shard: ShardId,
    next: u64,
    end: u64,
}

impl RowIdBlock {
    /// The number of ids that the block still has.
    pub fn remaining(&self) -> u64 {
        self.end - self.next
    }
}

impl Iterator for RowIdBlock {
    type Item = RowId;

    fn next(&mut self) -> Option<RowId> {
        if self.next == self.end {
            return None;
        }
        let id = RowId::new(self.shard, self.next);
        self.next += 1;
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks() {
        let ids = RowIds::new(ShardId(3), 5);
        let mut a = ids.take().unwrap();
        let b = ids.take().unwrap();
        assert_eq!((a.remaining(), ids.next_local()), (1024, 5 + 2048));
        let first = a.next().unwrap();
        assert_eq!((first.shard(), first.local(), a.remaining()), (ShardId(3), 5, 1023));
        assert_eq!(b.map(RowId::local).collect::<Vec<_>>(), (1029..2053).collect::<Vec<_>>());

        let ids = RowIds::new(ShardId(1), RowId::MAX_LOCAL - 9);
        let last = ids.take().unwrap();
        assert_eq!(last.remaining(), 10);
        assert_eq!(last.last().map(RowId::local), Some(RowId::MAX_LOCAL));
        let err = ids.take().unwrap_err();
        assert_eq!(err.state(), SqlState::PROGRAM_LIMIT_EXCEEDED);
    }
}
