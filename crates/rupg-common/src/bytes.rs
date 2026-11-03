//! `Bytes`, the bytes of a value that the value owns or that a mapping of a file holds, and `Mapping`, the bytes of a file in memory.
//!
//! The image of the store keeps the encoded columns of the tables. A load that maps the image reads the columns where the page cache holds them, with no copy, so that a column that no query reads costs no memory of the process (spec/19 section 19.5).

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

/// The bytes of a file in memory, which do not change while the mapping lives.
pub trait Mapping: Send + Sync + fmt::Debug {
    /// All the bytes of the mapping.
    fn bytes(&self) -> &[u8];

    /// Tells the operating system that the process does not need the pages of the range for now. A later read of the range reads the same bytes again. A mapping that cannot give back its pages does nothing.
    fn release(&self, _at: usize, _len: usize) {}
}

/// Bytes that a value owns, or a range of a [`Mapping`] that values share.
#[derive(Clone)]
pub struct Bytes(Repr);

#[derive(Clone)]
enum Repr {
    Owned(Box<[u8]>),
    Mapped { map: Arc<dyn Mapping>, at: usize, len: usize },
}

impl Bytes {
    /// The range `at..at + len` of `map`, or `None` when the range is not inside the mapping.
    pub fn mapped(map: &Arc<dyn Mapping>, at: usize, len: usize) -> Option<Bytes> {
        let end = at.checked_add(len)?;
        (end <= map.bytes().len()).then(|| Bytes(Repr::Mapped { map: Arc::clone(map), at, len }))
    }

    /// True when the bytes are in a mapping of a file.
    pub fn is_mapped(&self) -> bool {
        matches!(self.0, Repr::Mapped { .. })
    }

    /// Gives the pages of mapped bytes back to the operating system, after a read that took what it needs from them. Owned bytes stay.
    pub fn release(&self) {
        if let Repr::Mapped { map, at, len } = &self.0 {
            map.release(*at, *len);
        }
    }
}

impl Default for Bytes {
    fn default() -> Bytes {
        Bytes(Repr::Owned(Box::default()))
    }
}

impl Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.0 {
            Repr::Owned(bytes) => bytes,
            Repr::Mapped { map, at, len } => &map.bytes()[*at..*at + *len],
        }
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl From<Box<[u8]>> for Bytes {
    fn from(bytes: Box<[u8]>) -> Bytes {
        Bytes(Repr::Owned(bytes))
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(bytes: Vec<u8>) -> Bytes {
        Bytes(Repr::Owned(bytes.into_boxed_slice()))
    }
}

impl From<&[u8]> for Bytes {
    fn from(bytes: &[u8]) -> Bytes {
        Bytes(Repr::Owned(bytes.into()))
    }
}

impl PartialEq for Bytes {
    fn eq(&self, other: &Bytes) -> bool {
        **self == **other
    }
}

impl Eq for Bytes {}

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Repr::Owned(bytes) => write!(f, "Bytes({} owned)", bytes.len()),
            Repr::Mapped { len, .. } => write!(f, "Bytes({len} mapped)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mapping of bytes in memory, which counts its releases.
    #[derive(Debug)]
    struct Memory(Vec<u8>, std::sync::atomic::AtomicUsize);

    impl Mapping for Memory {
        fn bytes(&self) -> &[u8] {
            &self.0
        }

        fn release(&self, _at: usize, len: usize) {
            self.1.fetch_add(len, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[test]
    fn mapped_bytes_read_their_range_and_release_only_their_pages() {
        let memory = Arc::new(Memory((0..100).collect(), 0.into()));
        let map: Arc<dyn Mapping> = memory.clone();
        let bytes = Bytes::mapped(&map, 10, 5).unwrap();
        assert_eq!(&*bytes, &[10, 11, 12, 13, 14]);
        assert!(bytes.is_mapped());
        assert_eq!(bytes, Bytes::from(vec![10, 11, 12, 13, 14]));
        bytes.release();
        assert_eq!(memory.1.load(std::sync::atomic::Ordering::Relaxed), 5);
        Bytes::from(vec![1]).release();
        assert_eq!(memory.1.load(std::sync::atomic::Ordering::Relaxed), 5);
        assert!(Bytes::mapped(&map, 96, 5).is_none());
        assert!(Bytes::mapped(&map, usize::MAX, 2).is_none());
    }
}
