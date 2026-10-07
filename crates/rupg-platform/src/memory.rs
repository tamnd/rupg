//! `MemoryPool`, the one memory budget of the node (spec/04 section 4.6).
//!
//! The buffer pool, the undo buffers, the operator memory of each query, the plan cache and the catalog cache reserve their bytes here. A reservation that does not fit first asks the reclaimer, which is the buffer pool, to give back clean pages. If the bytes are still not there, the reservation fails with SQLSTATE `53200` and the caller spills to the file or stops the query.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use rupg_common::{Error, Result, SqlState};

/// The callback that frees memory. It gets the number of bytes that a reservation needs, frees what it can, and gives the number of bytes that it freed. It frees the bytes with [`Reservation::shrink`] on its own reservation.
pub type Reclaimer = Box<dyn Fn(u64) -> u64 + Send + Sync>;

/// A memory budget. The reservations are counted, not allocated.
pub struct MemoryPool {
    limit: AtomicU64,
    used: AtomicU64,
    /// The number of reservations since [`MemoryPool::fail_nth`].
    count: AtomicU64,
    /// The reservation with this count fails. 0 is off.
    fail_at: AtomicU64,
    reclaimer: RwLock<Option<Reclaimer>>,
}

impl fmt::Debug for MemoryPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryPool")
            .field("limit", &self.limit())
            .field("used", &self.used())
            .finish_non_exhaustive()
    }
}

impl MemoryPool {
    /// The default limit of the library: 256 MiB.
    pub const LIBRARY_DEFAULT: u64 = 256 << 20;

    /// A pool with a limit in bytes.
    pub fn new(limit: u64) -> Arc<MemoryPool> {
        Arc::new(MemoryPool {
            limit: AtomicU64::new(limit),
            used: AtomicU64::new(0),
            count: AtomicU64::new(0),
            fail_at: AtomicU64::new(0),
            reclaimer: RwLock::new(None),
        })
    }

    /// The default limit of the server: 25 percent of the physical memory, or the library default if the size of the memory is not known.
    pub fn server_default() -> u64 {
        crate::physical_memory().map_or(MemoryPool::LIBRARY_DEFAULT, |m| m / 4)
    }

    /// The limit in bytes.
    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    /// Changes the limit. Reservations that exist stay. New ones see the new limit.
    pub fn set_limit(&self, limit: u64) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    /// The bytes that the reservations hold now.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }

    /// Sets the callback that frees memory when a reservation does not fit. There is one callback for each pool.
    pub fn set_reclaimer(&self, reclaimer: Reclaimer) {
        *self.reclaimer.write().unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(reclaimer);
    }

    /// A test mode: the `n`th reservation from now fails, also when the bytes are free (spec/21 section 21.11). A test runs a query once for each `n` and checks that it fails cleanly. 0 turns the mode off.
    pub fn fail_nth(&self, n: u64) {
        self.count.store(0, Ordering::SeqCst);
        self.fail_at.store(n, Ordering::SeqCst);
    }

    /// Reserves `bytes`. The reservation gives the bytes back when it is dropped.
    pub fn reserve(self: &Arc<Self>, bytes: u64) -> Result<Reservation> {
        self.take(bytes)?;
        Ok(Reservation { pool: Arc::clone(self), bytes })
    }

    fn take(&self, bytes: u64) -> Result<()> {
        let fail_at = self.fail_at.load(Ordering::SeqCst);
        if fail_at > 0 && self.count.fetch_add(1, Ordering::SeqCst) + 1 == fail_at {
            return Err(self.refused(bytes, "the test mode failed this reservation"));
        }
        if self.try_take(bytes) {
            return Ok(());
        }
        let freed = match &*self.reclaimer.read().unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            Some(reclaim) => reclaim(bytes),
            None => 0,
        };
        if freed > 0 && self.try_take(bytes) {
            return Ok(());
        }
        Err(self.refused(bytes, "the reservations of the node hold the rest"))
    }

    fn try_take(&self, bytes: u64) -> bool {
        let limit = self.limit();
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|&n| n <= limit)
            })
            .is_ok()
    }

    fn give(&self, bytes: u64) {
        self.used.fetch_sub(bytes, Ordering::AcqRel);
    }

    fn refused(&self, bytes: u64, why: &str) -> Error {
        Error::new(SqlState::OUT_OF_MEMORY, "out of memory").with_detail(format!(
            "Failed on a request of {bytes} bytes. rupg.memory_limit is {} bytes and {} bytes are in use. {why}.",
            self.limit(),
            self.used()
        ))
    }
}

/// Bytes reserved from a [`MemoryPool`]. They go back to the pool on drop.
#[derive(Debug)]
pub struct Reservation {
    pool: Arc<MemoryPool>,
    bytes: u64,
}

impl Reservation {
    /// The bytes that this reservation holds.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Reserves `more` bytes. On an error the reservation does not change.
    pub fn grow(&mut self, more: u64) -> Result<()> {
        self.pool.take(more)?;
        self.bytes += more;
        Ok(())
    }

    /// Gives back `less` bytes, or all the bytes if it holds fewer.
    pub fn shrink(&mut self, less: u64) {
        let less = less.min(self.bytes);
        self.pool.give(less);
        self.bytes -= less;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.pool.give(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn reservations_count_against_the_limit() {
        let pool = MemoryPool::new(1000);
        let mut a = pool.reserve(600).unwrap();
        let e = pool.reserve(500).unwrap_err();
        assert_eq!(e.state(), SqlState::OUT_OF_MEMORY);
        assert!(e.detail().unwrap().contains("1000 bytes"), "{e}");
        let b = pool.reserve(400).unwrap();
        assert_eq!(pool.used(), 1000);
        assert!(a.grow(1).is_err());
        assert_eq!(a.bytes(), 600);
        a.shrink(100);
        a.grow(50).unwrap();
        assert_eq!(pool.used(), 950);
        drop(b);
        drop(a);
        assert_eq!(pool.used(), 0);
        assert!(pool.reserve(u64::MAX).is_err());
    }

    #[test]
    fn the_reclaimer_frees_memory() {
        let pool = MemoryPool::new(1000);
        // The buffer pool holds most of the budget.
        let cache = Arc::new(Mutex::new(pool.reserve(900).unwrap()));
        let held = Arc::clone(&cache);
        pool.set_reclaimer(Box::new(move |need| {
            let mut r = held.lock().unwrap();
            let free = need.min(r.bytes());
            r.shrink(free);
            free
        }));
        let query = pool.reserve(300).unwrap();
        assert_eq!(cache.lock().unwrap().bytes(), 600);
        assert_eq!(pool.used(), 900);
        drop(query);
        assert!(pool.reserve(2000).is_err());
    }

    #[test]
    fn the_nth_reservation_fails_in_the_test_mode() {
        let pool = MemoryPool::new(1 << 20);
        pool.fail_nth(3);
        let a = pool.reserve(1).unwrap();
        let b = pool.reserve(1).unwrap();
        assert_eq!(pool.reserve(1).unwrap_err().state(), SqlState::OUT_OF_MEMORY);
        let c = pool.reserve(1).unwrap();
        assert_eq!(pool.used(), 3);
        drop((a, b, c));
        pool.fail_nth(0);
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn the_server_default() {
        let d = MemoryPool::server_default();
        assert!(d >= 64 << 20, "{d}");
    }
}
