//! The one in-memory hash of the engine: the 64 bit finalizer of MurmurHash for a word, and eight bytes at a time for a string.
//!
//! A hash table of the engine uses [`Build`], so that two tables that share hashes always agree (spec/22 section 22.6). The hash is not keyed. It is for tables inside one process and never for a value that a user can see, so `hashint4` and `hashtext` of PostgreSQL are separate code.

// Lifted from rudb crates/rudb-kernels/src/hash.rs at cd9f9676 (2026-10-05).
// Changes: only the finalizer and the byte hash, with a Hasher and a BuildHasher around them. The value hash of DuckDB is not lifted.

use std::hash::{BuildHasher, Hasher};

const MULTIPLIER: u64 = 0xd6e8_feb8_6659_fd93;

/// The finalizer of MurmurHash64: every bit of the input moves every bit of the output.
#[must_use]
#[inline]
pub const fn word(mut x: u64) -> u64 {
    x ^= x >> 32;
    x = x.wrapping_mul(MULTIPLIER);
    x ^= x >> 32;
    x = x.wrapping_mul(MULTIPLIER);
    x ^ (x >> 32)
}

/// The hash of a byte string.
#[must_use]
#[inline]
pub fn bytes(data: &[u8]) -> u64 {
    let mut h = 0xe17a_1465_u64 ^ (data.len() as u64).wrapping_mul(0xc6a4_a793_5bd1_e995);
    let (blocks, rest) = data.as_chunks::<8>();
    for block in blocks {
        h = (h ^ u64::from_le_bytes(*block)).wrapping_mul(MULTIPLIER);
    }
    if !rest.is_empty() {
        let mut last = [0; 8];
        last[..rest.len()].copy_from_slice(rest);
        h = (h ^ u64::from_le_bytes(last)).wrapping_mul(MULTIPLIER);
    }
    word(h)
}

/// A [`Hasher`] over [`word`] and [`bytes`]. Each write folds into the state, and `finish` runs the finalizer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Fold {
    state: u64,
}

impl Hasher for Fold {
    #[inline]
    fn finish(&self) -> u64 {
        word(self.state)
    }

    #[inline]
    fn write(&mut self, data: &[u8]) {
        self.state = (self.state ^ bytes(data)).wrapping_mul(MULTIPLIER);
    }

    #[inline]
    fn write_u8(&mut self, value: u8) {
        self.write_u64(u64::from(value));
    }

    #[inline]
    fn write_u16(&mut self, value: u16) {
        self.write_u64(u64::from(value));
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.state = (self.state ^ value).wrapping_mul(MULTIPLIER).rotate_left(29);
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }

    #[inline]
    fn write_i64(&mut self, value: i64) {
        self.write_u64(value.cast_unsigned());
    }
}

/// The [`BuildHasher`] of every hash table of the engine.
#[derive(Clone, Copy, Debug, Default)]
pub struct Build;

impl BuildHasher for Build {
    type Hasher = Fold;

    #[inline]
    fn build_hasher(&self) -> Fold {
        Fold::default()
    }
}

/// A `HashMap` with [`Build`].
pub type HashMap<K, V> = std::collections::HashMap<K, V, Build>;

/// A `HashSet` with [`Build`].
pub type HashSet<K> = std::collections::HashSet<K, Build>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_spreads_small_keys() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..100_000u64 {
            assert!(seen.insert(word(i)));
        }
        assert_ne!(bytes(b""), bytes(b"\0"));
        assert_ne!(bytes(b"abcdefgh"), bytes(b"abcdefgh\0"));
    }

    #[test]
    fn a_map_with_the_build_works() {
        let mut map: HashMap<Vec<u8>, u32> = HashMap::default();
        for i in 0..1000u32 {
            map.insert(i.to_string().into_bytes(), i);
        }
        assert_eq!(map.get(b"517".as_slice()), Some(&517));
    }
}
