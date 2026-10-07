//! The loom tests of the latch (spec/21 section 21.11). Loom runs each test under every interleaving of its threads, and lets each load see each older value that the memory model allows.
//!
//! The module `latch` is `latch.rs` again, with the atomics of loom in place of the atomics of std. The pool keeps the atomics of std, because its state words are in the memory of the window. Run the tests with:
//!
//! ```sh
//! RUSTFLAGS="--cfg loom" cargo test --release -p rupg-buffer --lib loom
//! ```

use loom::cell::UnsafeCell;
use loom::sync::Arc;
use loom::sync::atomic::{AtomicU64, Ordering};
use loom::thread;

/// The atomics that `latch.rs` gets from its parent module.
use loom::sync::atomic;

// The second copy of latch.rs is the point of this module, see the module doc.
#[allow(dead_code, unreachable_pub, clippy::duplicate_mod)]
#[path = "latch.rs"]
mod latch;

use latch::{Latch, State, Word};

/// Tries once to take the exclusive latch of a frame that is unlocked or evicted, as a writer or a miss does. The tests do not wait for a latch: under loom a load in a wait loop can see the old value with no end, so each thread tries once and the test checks each outcome.
fn try_exclusive(l: &Latch) -> bool {
    let w = l.load();
    matches!(w.state(), State::Unlocked | State::Evicted) && l.try_exclusive(w)
}

/// A frame with a page that only a latch protects. Loom fails the test if two threads use the page at the same time with no order between them.
struct Frame {
    latch: Latch,
    page: UnsafeCell<u64>,
}

impl Frame {
    /// Adds 1 to the page under the exclusive latch, so the page counts the exclusive releases, as the version does. Gives false if the latch was not free.
    fn write(&self, evict: bool) -> bool {
        if !try_exclusive(&self.latch) {
            return false;
        }
        // SAFETY: the exclusive latch is held, so no other thread uses the page.
        self.page.with_mut(|p| unsafe { *p += 1 });
        if evict {
            self.latch.release_evicted()
        } else {
            self.latch.release_exclusive()
        }
        true
    }

    /// Reads the page under a shared latch. The page must match the version, because no writer can hold the latch at the same time.
    fn read(&self) -> bool {
        let w = self.latch.load();
        if !self.latch.try_shared(w) {
            return false;
        }
        // SAFETY: the shared latch is held, so no thread writes the page.
        let page = self.page.with(|p| unsafe { *p });
        assert_eq!(page, w.version());
        assert!(matches!(self.latch.load().state(), State::Shared(1 | 2)));
        self.latch.release_shared();
        true
    }
}

fn frame() -> Arc<Frame> {
    Arc::new(Frame { latch: Latch::evicted(), page: UnsafeCell::new(0) })
}

#[test]
fn writers_exclude_each_other_and_readers() {
    loom::model(|| {
        let f = frame();
        let a = {
            let f = f.clone();
            thread::spawn(move || f.write(false))
        };
        let b = {
            let f = f.clone();
            thread::spawn(move || f.write(true))
        };
        f.read();
        let writes = u64::from(a.join().unwrap()) + u64::from(b.join().unwrap());
        assert!(writes >= 1);
        // SAFETY: both writers are joined.
        assert_eq!(f.page.with(|p| unsafe { *p }), writes);
        let w = f.latch.load();
        assert_eq!(w.version(), writes);
        assert!(matches!(w.state(), State::Unlocked | State::Evicted));
    });
}

#[test]
fn readers_share_a_latch() {
    loom::model(|| {
        let f = frame();
        assert!(f.write(false));
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let f = f.clone();
                thread::spawn(move || f.read())
            })
            .collect();
        let wrote = f.write(false);
        let read: Vec<bool> = readers.into_iter().map(|r| r.join().unwrap()).collect();
        // A try fails only if another thread changed the word after the load. So if the writer failed, a reader held the latch.
        if !wrote {
            assert!(read.contains(&true));
        }
        let w = f.latch.load();
        assert_eq!((w.state(), w.version()), (State::Unlocked, 1 + u64::from(wrote)));
    });
}

/// A page of two words that a writer always sets to the same value.
struct Pair {
    latch: Latch,
    a: AtomicU64,
    b: AtomicU64,
}

#[test]
fn an_optimistic_read_sees_one_version() {
    loom::model(|| {
        let p =
            Arc::new(Pair { latch: Latch::evicted(), a: AtomicU64::new(0), b: AtomicU64::new(0) });
        assert!(try_exclusive(&p.latch));
        p.latch.release_exclusive();
        let writer = {
            let p = p.clone();
            thread::spawn(move || {
                assert!(try_exclusive(&p.latch));
                p.a.store(1, Ordering::Relaxed);
                p.b.store(1, Ordering::Relaxed);
                p.latch.release_evicted();
            })
        };
        // The reads of the page are relaxed, as the plain reads of a page in the pool are. If `still` passes, the two words must be the page of the version that the reader saw.
        let seen: Word = p.latch.load();
        if matches!(seen.state(), State::Unlocked | State::Shared(_)) {
            let a = p.a.load(Ordering::Relaxed);
            let b = p.b.load(Ordering::Relaxed);
            if p.latch.still(seen) {
                assert_eq!(seen.version(), 1);
                assert_eq!((a, b), (0, 0));
            }
        }
        writer.join().unwrap();
        assert!(!p.latch.still(seen));
    });
}
