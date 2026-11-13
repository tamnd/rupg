//! The link index and the CSR adjacency of document 13, lifted from rudb.
//!
//! A link stores a join between two tables as a function from row ids to row ids: the forward link of a child row gives the row id of its parent, a key map gives the row id of a parent key, and the backward adjacency gives the children of a parent. The modules are the structures of `rudb-graph` at `cd9f9676`, with the changes that each file's header gives. A [`Rid`] here is the local number of a rupg [`RowId`], the low 48 bits, because a link is inside one shard (document 13 section 13.7); [`row_id`] and [`local`] convert.
//!
//! Deleting every link changes no answer (document 13 section 13.1): a link is a faster way to compute what a hash join computes.
//!
//! This crate first ships in milestone M6. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![forbid(unsafe_code)]

pub mod adjacency;
pub mod bits;
pub mod degree;
pub mod keymap;
pub mod link;
pub mod rid;
pub mod rids;
pub mod span;
mod tail;
pub mod wire;

pub use adjacency::Adjacency;
pub use bits::BitVector;
pub use degree::{BUCKETS, Degrees};
pub use keymap::{DENSE_THRESHOLD, Form, KeyMap, Keys, Observed};
pub use link::{Bounds, Counts, Cursor, Link};
pub use rid::{NO_PARENT, PART_ROWS, Place, Places, Rid, STRIPE_PARTS};
pub use rids::{Pushed, Rids, SPARSE_RATIO, STOP_AFTER};
use rupg_common::{RowId, ShardId};
pub use span::Span;
pub use wire::Payload;

/// The row id of the row with local number `rid` in `shard`, or `None` for [`NO_PARENT`] and for a number past 48 bits.
#[must_use]
pub const fn row_id(shard: ShardId, rid: Rid) -> Option<RowId> {
    if rid == NO_PARENT {
        return None;
    }
    RowId::new(shard, rid)
}

/// The local number of a row id, the [`Rid`] that a link of its shard stores.
#[must_use]
pub const fn local(row: RowId) -> Rid {
    row.local()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_ids_keep_their_shard_and_local_number() {
        let row = row_id(ShardId(3), 41).expect("fits");
        assert_eq!(row.shard(), ShardId(3));
        assert_eq!(local(row), 41);
        assert_eq!(row_id(ShardId(0), NO_PARENT), None);
        assert_eq!(row_id(ShardId(0), RowId::MAX_LOCAL + 1), None);
    }
}
