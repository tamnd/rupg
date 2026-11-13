// Lifted from rudb crates/rudb-graph/src/tail.rs at cd9f9676 (2026-10-05).
// Changes: errors of malformed bytes are XX001 `Error::corrupted`.

//! The back of a buffer a structure was read from, kept rather than copied.
//!
//! A forward link or an adjacency is read out of a section's payload, and most of the payload is
//! the packed rows at the end of it. Copying those rows into a buffer of their own wrote every one
//! of them to fresh memory a second time, and on TPC-H q21 loading the sections was about half the
//! page faults of a query whose system time was as large as its user time. Taking the payload
//! instead keeps the rows where the read put them, at the cost of the header in front of them.

use std::ops::Deref;

/// Bytes from `at` to the end of `held`.
#[derive(Debug, Default)]
pub(crate) struct Tail {
    held: Vec<u8>,
    at: usize,
}

impl Tail {
    /// The bytes of `held` from `at` on, or `None` when `at` is past its end.
    pub(crate) fn of(held: Vec<u8>, at: usize) -> Option<Self> {
        (at <= held.len()).then_some(Self { held, at })
    }
}

impl From<Vec<u8>> for Tail {
    fn from(held: Vec<u8>) -> Self {
        Self { held, at: 0 }
    }
}

impl Deref for Tail {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.held[self.at..]
    }
}

/// A copy of the tail alone, since the header in front of it is not what a clone is asked for.
impl Clone for Tail {
    fn clone(&self) -> Self {
        Self::from(self.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::Tail;

    #[test]
    fn a_tail_is_the_bytes_past_its_start_and_a_clone_holds_only_those() {
        let tail = Tail::of(vec![1, 2, 3, 4, 5], 2).expect("inside the buffer");
        assert_eq!(&*tail, &[3, 4, 5]);
        let copy = tail.clone();
        assert_eq!(&*copy, &[3, 4, 5]);
        assert_eq!(copy.held.len(), 3);
        assert!(Tail::of(vec![1, 2], 3).is_none());
        assert!(Tail::of(vec![1, 2], 2).is_some_and(|tail| tail.is_empty()));
    }
}
