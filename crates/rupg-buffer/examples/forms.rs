//! The cost of a page hit in the two forms of the buffer pool, for Q38 of spec/24.
//!
//! A host that cannot reserve the address space of the window uses the table of spec/08 section 8.8.4, where a hash map finds the frame of a page. This program forces each form on the same host, so it gives the cost of the table against the window without a host with a 39-bit address space.
//!
//! Usage: `forms [PAGES] [THREADS]`. The program loads PAGES pages (16,384) into a pool that holds them all, so each access after the first pass is a hit. Then it times random optimistic reads and random shared latches, first with one thread and then with THREADS threads (one for each core). It prints the nanoseconds of each access.

// The program times the pool on the threads of the host. It is a measure and not part of the library, so it uses std::time and std::thread and not the traits of rupg-platform.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Instant;

use rupg_buffer::{BufferPool, MemStore, PoolConfig, PoolMode, Store};
use rupg_file::{OptimisticRead, PAGE_SIZE, PageAccess, PageId};
use rupg_platform::MemoryPool;

/// The accesses of each thread in one timed pass.
const ACCESSES: u64 = 4_000_000;

/// A small generator, so that each thread reads the pages in a different order.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn pool(store: &Arc<MemStore>, pages: u64, mode: PoolMode) -> Arc<BufferPool> {
    let bytes = pages * PAGE_SIZE as u64;
    // The budget holds the frames and their headers with room to spare, so no page is evicted.
    let memory = MemoryPool::new(bytes * 2);
    let config = PoolConfig {
        shared_buffers: bytes,
        mode: Some(mode),
        window_pages: Some(pages.next_power_of_two() * 2),
    };
    BufferPool::new(Arc::clone(store) as Arc<dyn Store>, &memory, config).expect("pool")
}

/// The nanoseconds of one access, over `threads` threads that each make `ACCESSES` accesses.
fn time(pool: &BufferPool, pages: u64, threads: u64, latch: bool) -> f64 {
    let start = Instant::now();
    std::thread::scope(|s| {
        for t in 0..threads {
            s.spawn(move || {
                let mut state = 0x9e37_79b9_7f4a_7c15 ^ (t + 1);
                let mut sum = 0u64;
                let mut word = [0u8; 8];
                for _ in 0..ACCESSES {
                    let page = PageId(1 + next(&mut state) % pages);
                    if latch {
                        let g = pool.shared(page).expect("shared");
                        sum = sum.wrapping_add(u64::from(g[0]));
                    } else {
                        let g = pool.optimistic(page).expect("optimistic");
                        if g.read(0, &mut word) {
                            sum = sum.wrapping_add(u64::from_le_bytes(word));
                        }
                    }
                }
                std::hint::black_box(sum);
            });
        }
    });
    // The threads run at the same time, so the wall time of one pass over the accesses of one thread is the time of each access.
    start.elapsed().as_nanos() as f64 / ACCESSES as f64
}

fn main() {
    let args: Vec<u64> = std::env::args().skip(1).map(|a| a.parse().expect("a number")).collect();
    let pages = args.first().copied().unwrap_or(16_384);
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get() as u64);
    let threads = args.get(1).copied().unwrap_or(cores);
    let store = Arc::new(MemStore::new());
    for _ in 0..pages {
        let p = store.allocate().expect("allocate");
        let mut page = Box::new([0u8; PAGE_SIZE]);
        page[..8].copy_from_slice(&p.0.to_le_bytes());
        store.write(p, &page).expect("write");
    }
    println!(
        "{} {}, {pages} pages, ns for each access of one thread",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("form    threads  optimistic  shared");
    for mode in [PoolMode::Window, PoolMode::Table] {
        let pool = pool(&store, pages, mode);
        assert_eq!(pool.mode(), mode);
        for p in 1..=pages {
            drop(pool.shared(PageId(p)).expect("load"));
        }
        for n in [1, threads] {
            let o = time(&pool, pages, n, false);
            let s = time(&pool, pages, n, true);
            println!("{mode:<7} {n:>7}  {o:>10.1}  {s:>6.1}");
        }
        assert_eq!(pool.stats().misses, pages, "a page was evicted");
    }
}
