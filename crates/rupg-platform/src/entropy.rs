//! The `Entropy` trait.

use std::fmt;

/// The source of random bytes, for example for the cluster id of a new file and for the salt of a SCRAM password.
pub trait Entropy: Send + Sync + fmt::Debug {
    /// Fills `buf` with random bytes.
    fn fill(&self, buf: &mut [u8]);

    /// A random `u64`.
    fn next_u64(&self) -> u64 {
        let mut b = [0; 8];
        self.fill(&mut b);
        u64::from_le_bytes(b)
    }
}
