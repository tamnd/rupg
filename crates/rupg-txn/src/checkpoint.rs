//! The page filter and the checkpoint of the hot pages (spec/11 section 11.15).
//!
//! The buffer pool writes a page out of place, and the file shows the pages of the last checkpoint after a crash. Recovery replays the log from the redo position of that checkpoint, and it does not undo. So a page in the file must hold no version that is not committed, and no version of a commit that the log after the redo position does not hold.
//!
//! The filter does this. Before the pool writes a hot leaf, the filter puts in each row the version that a snapshot `W` sees, and leaves out a row that `W` does not see. Each commit at or below `visible` has a safe block, because a commit installs its timestamp after its block is safe. So `W` is `visible` at the write. A page that the filter changed stays dirty in the pool, so the clock does not evict it.
//!
//! A checkpoint stops the changes to the pages, takes `W` from `visible`, writes each dirty page with the versions of `W`, and moves the redo position of the ring to the end of the last block at or below `W`. The blocks after that position are all above `W`. At M1 the checkpoint stops the writers while it writes the pages. Writers that wait for it hold no page latch.

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, Weak};

use rupg_buffer::{BufferPool, Checkpoint, FileStore, Filtered, PageFilter};
use rupg_common::{Hlc, Result};
use rupg_file::{Page, PageId};
use rupg_hot::{Rewrite, rewrite_leaf};
use rupg_log::Ring;

use crate::txn::{RING, Transactions};

/// The filter of the pool: it writes the versions that a snapshot at `visible` sees.
#[derive(Debug)]
struct Visible(Weak<Transactions<BufferPool>>);

impl PageFilter for Visible {
    fn filter(&self, _: PageId, copy: &mut Page) -> Result<Filtered> {
        // When the transactions are gone, no row has an owner or an undo record, and no commit is in progress.
        let Some(txns) = self.0.upgrade() else { return Ok(Filtered::Same) };
        let at = txns.filter_at.load(Ordering::Acquire);
        let (w, held) = match at {
            0 => (txns.hold(), true),
            at => (Hlc::from_bits(at), false),
        };
        let rewrite = rewrite_leaf(copy, w, |row| txns.version_at(row, w));
        if held {
            txns.release(w);
        }
        // Older versions can be longer. If they do not fit, the page waits for a later write, and a checkpoint fails.
        Ok(match rewrite? {
            Rewrite::Same => Filtered::Same,
            Rewrite::Changed => Filtered::Changed,
            Rewrite::NoRoom => Filtered::Skip,
        })
    }
}

impl Transactions<BufferPool> {
    /// Sets the filter of the pool, so that the pool writes only versions that are committed and safe in the log. Call it once, before the first transaction.
    pub fn filter_writes(self: &Arc<Self>) -> Result<()> {
        self.pages().set_filter(Arc::new(Visible(Arc::downgrade(self))))
    }

    /// Writes a checkpoint of the pages and the log to `store`, with the other roots from `base`, and gives its snapshot `W`. After a crash, the file holds the rows of each commit at or below `W`, and recovery replays the blocks above `W`.
    ///
    /// The changes to the pages wait while it runs. If a page cannot be written with the versions of `W`, the result is an error and the last checkpoint stays.
    pub fn checkpoint(&self, store: &FileStore, base: &Checkpoint) -> Result<Hlc> {
        let _quiet = self.quiet.write().unwrap_or_else(PoisonError::into_inner);
        let w = self.hold();
        self.filter_at.store(w.bits(), Ordering::Release);
        let done = self.write_checkpoint(store, base, w);
        self.filter_at.store(0, Ordering::Release);
        self.release(w);
        done.map(|()| w)
    }

    fn write_checkpoint(&self, store: &FileStore, base: &Checkpoint, w: Hlc) -> Result<()> {
        self.pages().flush_all()?;
        let rings = match self.log() {
            Some(log) => {
                let _gate = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
                let mut placed = self.placed.lock().unwrap_or_else(PoisonError::into_inner);
                let below = placed.iter().take_while(|&&(ts, _)| ts <= w).count();
                if let Some(&(_, end)) = below.checked_sub(1).and_then(|last| placed.get(last)) {
                    log.ring(RING)?.set_redo(end)?;
                }
                placed.drain(..below);
                log.rings().iter().map(Ring::state).collect()
            }
            None => store.rings(),
        };
        store.checkpoint(&Checkpoint { timestamp: w, rings, ..base.clone() })
    }
}
