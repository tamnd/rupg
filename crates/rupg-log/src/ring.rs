//! One log ring: block placement, group commit and the reader (spec/11 sections 11.13 and 11.14.1).
//!
//! A ring position is a byte offset that only increases. The physical place of a position is the position modulo the ring size, mapped through the list of extents. A block never spans two extents. A flush pads the last 4 KiB unit, so the next flush starts on a new unit and never writes a unit that is already durable.

use std::sync::{Arc, PoisonError};

use rupg_buffer::{FileStore, RingState};
use rupg_common::{Error, Hlc, Result, SqlState};
use rupg_file::RING_EXTENT_BYTES;

use crate::block::{BLOCK_HEADER, Block, BlockHeader, UNIT};
use crate::sync::{Condvar, IoMark, Mutex, MutexGuard};

/// The place of a block in its ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    /// The ring position of the first byte.
    pub position: u64,
    /// The ring position just after the block.
    pub end: u64,
}

/// Bytes that wait for the next flush. Each piece starts on a unit.
#[derive(Debug)]
struct Piece {
    at: u64,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct State {
    redo: u64,
    end: u64,
    durable: u64,
    /// The newest commit timestamp in the ring, and the newest one that is durable.
    newest: Hlc,
    durable_newest: Hlc,
    pending: Vec<Piece>,
    flushing: bool,
    /// A failed flush stops the ring, because the bytes of the flush may be on disk or not.
    failed: Option<Error>,
}

/// One log ring. Many threads can append to a ring at the same time, and one of the threads that wait for a position does the flush for all of them.
#[derive(Debug)]
pub struct Ring {
    store: Arc<FileStore>,
    number: u16,
    worker: u32,
    extents: Vec<u64>,
    state: Mutex<State>,
    flushed: Condvar,
    io: IoMark,
}

fn ring_number(n: u32) -> Result<u16> {
    u16::try_from(n).map_err(|_| Error::internal(format!("the ring number {n} is above 65535")))
}

impl Ring {
    /// A new ring of `extents` extents with all positions at zero. The extents come from the store. They are durable after a checkpoint that names the ring.
    pub fn create(store: Arc<FileStore>, number: u32, worker: u32, extents: usize) -> Result<Ring> {
        if extents == 0 {
            return Err(Error::internal("a log ring needs at least one extent"));
        }
        let number = ring_number(number)?;
        let mut list = Vec::with_capacity(extents);
        for _ in 0..extents {
            list.push(store.add_ring_extent()?);
        }
        Ok(Ring::new(store, number, worker, list, 0, 0, Hlc::ZERO))
    }

    /// The ring of a checkpoint. The next block goes at the durable position, on a new unit. Recovery gives the state, see [`crate::Recovery::finish`].
    pub fn open(store: Arc<FileStore>, state: &RingState) -> Result<Ring> {
        let number = ring_number(state.ring)?;
        if state.extents.is_empty()
            || state.durable < state.redo
            || state.durable - state.redo > state.size()
        {
            return Err(Error::internal(format!(
                "ring {} cannot open with the redo position {} and the durable position {}",
                state.ring, state.redo, state.durable
            )));
        }
        let (redo, end, newest) = (state.redo, state.durable, state.newest);
        Ok(Ring::new(store, number, state.worker, state.extents.clone(), redo, end, newest))
    }

    fn new(
        store: Arc<FileStore>,
        number: u16,
        worker: u32,
        extents: Vec<u64>,
        redo: u64,
        end: u64,
        newest: Hlc,
    ) -> Ring {
        Ring {
            store,
            number,
            worker,
            extents,
            state: Mutex::new(State {
                redo,
                end: end.next_multiple_of(UNIT),
                durable: end,
                newest,
                durable_newest: newest,
                pending: Vec::new(),
                flushing: false,
                failed: None,
            }),
            flushed: Condvar::new(),
            io: IoMark::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The ring number.
    pub fn number(&self) -> u16 {
        self.number
    }

    /// The file store of the ring.
    pub fn store(&self) -> &FileStore {
        &self.store
    }

    /// The ring size in bytes.
    pub fn size(&self) -> u64 {
        self.extents.len() as u64 * RING_EXTENT_BYTES
    }

    /// The position after the last block.
    pub fn end(&self) -> u64 {
        self.lock().end
    }

    /// The durable position.
    pub fn durable(&self) -> u64 {
        self.lock().durable
    }

    /// The state for a checkpoint, with the durable position of now.
    pub fn state(&self) -> RingState {
        let s = self.lock();
        RingState {
            ring: u32::from(self.number),
            worker: self.worker,
            redo: s.redo,
            durable: s.durable,
            newest: s.durable_newest,
            extents: self.extents.clone(),
        }
    }

    /// Moves the redo position to `redo`, which the caller took from the safe position before the checkpoint. The space before it can take new blocks after the checkpoint is durable.
    pub fn set_redo(&self, redo: u64) -> Result<()> {
        let mut s = self.lock();
        if redo < s.redo || redo > s.durable {
            return Err(Error::internal(format!(
                "ring {} cannot move its redo position from {} to {redo}, its durable position is {}",
                self.number, s.redo, s.durable
            )));
        }
        s.redo = redo;
        Ok(())
    }

    /// Places `block` at the end of the ring. It sets the ring number and the position of the block. The block is durable after a [`Ring::flush_to`] of its end. A ring with no room gives SQLSTATE `53100`, and a checkpoint must move the redo position first.
    pub fn append(&self, block: &mut Block) -> Result<Placed> {
        let len = block.len() as u64;
        let mut s = self.lock();
        if let Some(e) = &s.failed {
            return Err(e.clone());
        }
        let mut at = s.end;
        let left = UNIT - at % UNIT;
        if left < UNIT && left < BLOCK_HEADER as u64 {
            s.pad_unit();
            at += left;
        }
        let left = RING_EXTENT_BYTES - at % RING_EXTENT_BYTES;
        let fill = len > left;
        let position = if fill { at + left } else { at };
        if position + len - s.redo > self.size() {
            return Err(Error::new(
                SqlState::DISK_FULL,
                format!("log ring {} is full", self.number),
            )
            .with_detail(format!(
                "The ring holds {} bytes from position {}, and a block of {len} bytes does not fit.",
                self.size(),
                s.redo
            ))
            .with_hint("A checkpoint frees the ring space before the redo position."));
        }
        block.ring = self.number;
        block.position = position;
        let mut bytes = Vec::with_capacity(block.len());
        block.encode(&mut bytes)?;
        if fill {
            let header = Block::fill(self.number, at, left as usize)?;
            s.put(at, &header);
            s.pad_unit();
        }
        s.put(position, &bytes);
        s.end = position + len;
        s.newest = s.newest.max(block.commit_ts);
        Ok(Placed { position, end: position + len })
    }

    /// Makes the ring durable at least up to `position` and gives the durable position. A thread that finds no flush running does the flush for every block that is placed, and the others wait for it. An error stops the ring, and each later call gives the same error.
    pub fn flush_to(&self, position: u64) -> Result<u64> {
        let mut s = self.lock();
        if position > s.end {
            return Err(Error::internal(format!(
                "ring {} cannot flush to {position}, beyond its end {}",
                self.number, s.end
            )));
        }
        loop {
            if let Some(e) = &s.failed {
                return Err(e.clone());
            }
            if s.durable >= position {
                return Ok(s.durable);
            }
            if s.flushing {
                s = self.flushed.wait(s).unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            let end = s.end;
            if !end.is_multiple_of(UNIT) {
                let left = UNIT - end % UNIT;
                if left >= BLOCK_HEADER as u64 {
                    let header = Block::fill(self.number, end, left as usize)?;
                    s.put(end, &header);
                }
                s.pad_unit();
                s.end = end + left;
            }
            let pieces = std::mem::take(&mut s.pending);
            let (upto, newest) = (s.end, s.newest);
            s.flushing = true;
            drop(s);
            let result = self.write(&pieces).and_then(|()| {
                self.io.touch();
                self.store.sync_rings()
            });
            s = self.lock();
            s.flushing = false;
            match result {
                Ok(()) => {
                    s.durable = upto;
                    s.durable_newest = newest;
                }
                Err(e) => s.failed = Some(e),
            }
            self.flushed.notify_all();
        }
    }

    /// Writes the pieces, split at the extent bounds.
    fn write(&self, pieces: &[Piece]) -> Result<()> {
        for p in pieces {
            let mut done = 0;
            while done < p.bytes.len() {
                let (extent, offset) = self.place(p.at + done as u64);
                let n = ((RING_EXTENT_BYTES - offset) as usize).min(p.bytes.len() - done);
                self.io.touch();
                self.store.write_ring(extent, offset, &p.bytes[done..done + n])?;
                done += n;
            }
        }
        Ok(())
    }

    /// The extent and the byte offset in it of a ring position.
    fn place(&self, position: u64) -> (u64, u64) {
        place(&self.extents, position)
    }
}

fn place(extents: &[u64], position: u64) -> (u64, u64) {
    let size = extents.len() as u64 * RING_EXTENT_BYTES;
    let at = position % size;
    (extents[(at / RING_EXTENT_BYTES) as usize], at % RING_EXTENT_BYTES)
}

impl State {
    /// Adds bytes at `at` to the bytes that wait for the flush.
    fn put(&mut self, at: u64, bytes: &[u8]) {
        match self.pending.last_mut() {
            Some(p) if p.at + p.bytes.len() as u64 == at => p.bytes.extend_from_slice(bytes),
            _ => {
                debug_assert!(at.is_multiple_of(UNIT));
                self.pending.push(Piece { at, bytes: bytes.to_vec() });
            }
        }
    }

    /// Adds zero bytes to the last piece up to the end of its unit, so that a flush writes whole units.
    fn pad_unit(&mut self) {
        if let Some(p) = self.pending.last_mut() {
            p.bytes.resize(p.bytes.len().next_multiple_of(UNIT as usize), 0);
        }
    }
}

/// Reads the blocks of a ring from a position until the end of the ring (spec/11 section 11.16 step 2). It skips fill blocks and the bytes at the end of a unit where no header fits.
#[derive(Debug)]
pub struct RingReader<'a> {
    store: &'a FileStore,
    ring: u16,
    extents: &'a [u64],
    at: u64,
    limit: u64,
    chunk_at: u64,
    /// The bytes of `chunk` that hold the file from `chunk_at`. The buffer keeps its size, so a later read does not zero it again.
    chunk_len: usize,
    chunk: Vec<u8>,
    bodies: bool,
    done: bool,
}

/// The bytes that the reader reads at a time, unless the extent ends first.
const CHUNK: u64 = 256 << 10;

impl<'a> RingReader<'a> {
    /// A reader of the ring of `state` from `from`, which is usually the redo position of the checkpoint.
    pub fn new(store: &'a FileStore, state: &'a RingState, from: u64) -> Result<RingReader<'a>> {
        Ok(RingReader {
            store,
            ring: ring_number(state.ring)?,
            extents: &state.extents,
            at: from,
            limit: from + state.size(),
            chunk_at: 0,
            chunk_len: 0,
            chunk: Vec::new(),
            bodies: true,
            done: state.extents.is_empty(),
        })
    }

    /// The same reader, but each block that it gives has an empty body. It still reads each block and checks its checksum. Recovery uses it for the scan, which keeps only the headers.
    pub fn without_bodies(mut self) -> RingReader<'a> {
        self.bodies = false;
        self
    }

    /// The position after the last block that the reader gave. When the reader is at the end, it is the end of the ring.
    pub fn position(&self) -> u64 {
        self.at
    }

    /// `len` bytes at ring position `at`, which do not cross an extent.
    fn bytes(&mut self, at: u64, len: usize) -> Result<&[u8]> {
        let end = at + len as u64;
        if at < self.chunk_at || end > self.chunk_at + self.chunk_len as u64 {
            let (extent, offset) = place(self.extents, at);
            let start = offset - offset % UNIT;
            let want =
                (offset - start + len as u64).max(CHUNK).min(RING_EXTENT_BYTES - start) as usize;
            if self.chunk.len() < want {
                self.chunk.resize(want, 0);
            }
            self.chunk_len = 0;
            self.store.read_ring(extent, start, &mut self.chunk[..want])?;
            self.chunk_len = want;
            self.chunk_at = at - (offset - start);
        }
        let from = (at - self.chunk_at) as usize;
        Ok(&self.chunk[from..from + len])
    }

    /// The next block that is not a fill block, with its place, or `None` at the end of the ring. A block that passes its checksum and is still not valid gives SQLSTATE `XX001`.
    pub fn next_block(&mut self) -> Result<Option<(Placed, Block)>> {
        while !self.done {
            let left = UNIT - self.at % UNIT;
            if left < BLOCK_HEADER as u64 {
                self.at += left;
            }
            let at = self.at;
            let room = RING_EXTENT_BYTES - at % RING_EXTENT_BYTES;
            let mut header = [0; BLOCK_HEADER];
            header.copy_from_slice(self.bytes(at, BLOCK_HEADER)?);
            let h = match BlockHeader::read(&header) {
                Some(h) if h.length as u64 <= room && at + h.length as u64 <= self.limit => h,
                _ => break,
            };
            let ring = self.ring;
            let bodies = self.bodies;
            let block = if h.fill {
                Block::decode(&header, ring, at)?
            } else {
                Block::decode_with(self.bytes(at, h.length)?, ring, at, bodies)?
            };
            let Some(block) = block else { break };
            self.at = at + h.length as u64;
            if !h.fill {
                return Ok(Some((Placed { position: at, end: self.at }, block)));
            }
        }
        self.done = true;
        Ok(None)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use rupg_buffer::Checkpoint;
    use rupg_platform::sim::SimIo;
    use rupg_platform::{Io, OpenMode};

    use super::*;
    use crate::block::{BlockKind, MAX_BLOCK};

    pub(crate) fn ts(n: u64) -> Hlc {
        Hlc::new(1000 + n, 0).unwrap()
    }

    pub(crate) fn store(io: &SimIo) -> Arc<FileStore> {
        let file = io.open(Path::new("db"), OpenMode::CreateNew).unwrap();
        Arc::new(FileStore::create(file, [3; 16], ts(0)).unwrap())
    }

    pub(crate) fn commit(xid: u64, body: usize) -> Block {
        Block {
            kind: BlockKind::Commit,
            ring: 0,
            position: 0,
            commit_ts: ts(xid),
            xid,
            deps: Vec::new(),
            body: (0..body).map(|i| (i as u64 * 31 + xid) as u8).collect(),
        }
    }

    /// Every block of a ring from `from`, and the end.
    pub(crate) fn read_all(store: &FileStore, state: &RingState, from: u64) -> (Vec<Block>, u64) {
        let mut r = RingReader::new(store, state, from).unwrap();
        let mut out = Vec::new();
        while let Some((placed, b)) = r.next_block().unwrap() {
            assert_eq!(placed.position, b.position);
            out.push(b);
        }
        (out, r.position())
    }

    #[test]
    fn append_and_read() {
        let io = SimIo::new(1);
        let store = store(&io);
        let ring = Ring::create(store.clone(), 4, 9, 1).unwrap();
        assert_eq!((ring.number(), ring.size(), ring.end()), (4, RING_EXTENT_BYTES, 0));
        let mut written = Vec::new();
        for xid in 1..=20 {
            let mut b = commit(xid, xid as usize * 37);
            let placed = ring.append(&mut b).unwrap();
            assert_eq!((placed.position, b.ring), (b.position, 4));
            assert_eq!(placed.end, placed.position + b.len() as u64);
            written.push(b);
        }
        // Nothing is durable before the flush, and one flush syncs once.
        assert_eq!(ring.durable(), 0);
        let syncs = io.syncs();
        let end = ring.end();
        let durable = ring.flush_to(end).unwrap();
        assert_eq!(io.syncs(), syncs + 1);
        assert_eq!(durable, end.next_multiple_of(UNIT));
        assert_eq!(ring.flush_to(end).unwrap(), durable);
        assert_eq!(io.syncs(), syncs + 1);

        let state = ring.state();
        assert_eq!(
            (state.ring, state.worker, state.durable, state.newest),
            (4, 9, durable, ts(20))
        );
        let (read, at) = read_all(&store, &state, 0);
        assert_eq!(read.len(), 20);
        for (r, w) in read.iter().zip(&written) {
            assert_eq!(
                (r.position, r.xid, &r.body[..w.body.len()]),
                (w.position, w.xid, &w.body[..])
            );
        }
        assert_eq!(at, durable);

        // The next block starts on a new unit.
        let placed = ring.append(&mut commit(21, 10)).unwrap();
        assert_eq!(placed.position, durable);
        ring.flush_to(placed.end).unwrap();
        assert_eq!(read_all(&store, &ring.state(), 0).0.len(), 21);
    }

    #[test]
    fn unit_and_extent_ends() {
        let io = SimIo::new(2);
        let store = store(&io);
        let ring = Ring::create(store.clone(), 0, 0, 3).unwrap();
        // A block that leaves 32 bytes in its unit. The next block starts on the next unit.
        let first = ring.append(&mut commit(1, UNIT as usize - 72)).unwrap();
        assert_eq!(first.end, UNIT - 32);
        let second = ring.append(&mut commit(2, 8)).unwrap();
        assert_eq!(second.position, UNIT);
        // A large block does not fit in the rest of the first extent, so it goes to the second.
        let big = MAX_BLOCK - BLOCK_HEADER;
        let third = ring.append(&mut commit(3, big)).unwrap();
        assert_eq!(third.position, RING_EXTENT_BYTES);
        let fourth = ring.append(&mut commit(4, 8)).unwrap();
        assert_eq!(fourth.position, 2 * RING_EXTENT_BYTES);
        let durable = ring.flush_to(fourth.end).unwrap();
        let (read, at) = read_all(&store, &ring.state(), 0);
        assert_eq!(read.iter().map(|b| b.xid).collect::<Vec<_>>(), [1, 2, 3, 4]);
        assert_eq!(at, durable);
        assert_eq!(ring.place(fourth.position), (ring.extents[2], 0));
    }

    #[test]
    fn a_full_ring() {
        let io = SimIo::new(3);
        let store = store(&io);
        let ring = Ring::create(store.clone(), 0, 0, 1).unwrap();
        let half = (RING_EXTENT_BYTES / 2) as usize - BLOCK_HEADER;
        let a = ring.append(&mut commit(1, half)).unwrap();
        let b = ring.append(&mut commit(2, half)).unwrap();
        assert_eq!(b.end, ring.size());
        let e = ring.append(&mut commit(3, 100)).unwrap_err();
        assert_eq!(e.state(), SqlState::DISK_FULL);
        // A block fits after the redo position moves, and it takes the place of the first block.
        ring.flush_to(b.end).unwrap();
        assert!(ring.set_redo(b.end + UNIT).is_err());
        ring.set_redo(a.end).unwrap();
        assert!(ring.set_redo(0).is_err());
        let c = ring.append(&mut commit(3, 100)).unwrap();
        assert_eq!(c.position, ring.size());
        assert_eq!(ring.place(c.position), ring.place(0));
        ring.flush_to(c.end).unwrap();
        let (read, _) = read_all(&store, &ring.state(), a.end);
        assert_eq!(read.iter().map(|b| b.xid).collect::<Vec<_>>(), [2, 3]);
        // A read from position 0 finds the third block there, so it is at the end at once.
        assert_eq!(read_all(&store, &ring.state(), 0).0.len(), 0);
    }

    #[test]
    fn a_failed_flush_stops_the_ring() {
        let io = SimIo::new(4);
        let store = store(&io);
        let ring = Ring::create(store.clone(), 0, 0, 1).unwrap();
        let a = ring.append(&mut commit(1, 10)).unwrap();
        store.free_ring_extent(ring.extents[0]).unwrap();
        let e = ring.flush_to(a.end).unwrap_err();
        assert_eq!(ring.flush_to(a.end).unwrap_err(), e);
        assert_eq!(ring.append(&mut commit(2, 10)).unwrap_err(), e);
        assert!(ring.flush_to(a.end + UNIT * 10).is_err());
    }

    #[test]
    fn open_after_a_checkpoint() {
        let io = SimIo::new(5);
        let store = store(&io);
        let ring = Ring::create(store.clone(), 1, 2, 1).unwrap();
        let a = ring.append(&mut commit(1, 10)).unwrap();
        ring.flush_to(a.end).unwrap();
        store
            .checkpoint(&Checkpoint {
                timestamp: ts(5),
                rings: vec![ring.state()],
                ..Checkpoint::default()
            })
            .unwrap();
        let b = ring.append(&mut commit(2, 10)).unwrap();
        ring.flush_to(b.end).unwrap();
        drop(ring);

        let state = store.rings()[0].clone();
        let (read, end) = read_all(&store, &state, state.redo);
        assert_eq!(read.iter().map(|b| b.xid).collect::<Vec<_>>(), [1, 2]);
        // Recovery opens the ring one ring size after its end.
        let start = end + state.size();
        let next = RingState { redo: start, durable: start, newest: ts(2), ..state.clone() };
        let ring = Ring::open(store.clone(), &next).unwrap();
        assert_eq!((ring.durable(), ring.end(), ring.state()), (start, start, next.clone()));
        let c = ring.append(&mut commit(3, 10)).unwrap();
        assert_eq!(c.position, start);
        ring.flush_to(c.end).unwrap();
        assert_eq!(read_all(&store, &ring.state(), start).0.len(), 1);
        // The new block is in the physical place after the old blocks, but a read from the old redo position does not take it.
        let (old, old_end) = read_all(&store, &state, state.redo);
        assert_eq!((old.len(), old_end), (2, end));
        for bad in [
            RingState { redo: 16, durable: 8, ..state.clone() },
            RingState { redo: 0, durable: state.size() + 8, ..state.clone() },
            RingState { extents: Vec::new(), ..state.clone() },
            RingState { ring: 70_000, ..state.clone() },
        ] {
            assert!(Ring::open(store.clone(), &bad).is_err());
        }
    }
}

/// The loom test of the reservation of space in a ring and of group commit (spec/21 section 21.11). The ring takes the lock and the condition variable of loom, and the store on the simulated disk has no loom operation, so loom sees each write and each sync as one step. Run it with:
///
/// ```sh
/// RUSTFLAGS="--cfg loom" cargo test --release -p rupg-log --lib loom
/// ```
#[cfg(all(test, loom))]
mod loom_tests {
    use loom::sync::Arc;
    use loom::thread;
    use rupg_platform::sim::SimIo;

    use super::tests::{commit, read_all, store};
    use super::*;

    /// Each thread places one block and flushes to its end. One thread can flush the block of another, or wait while another flushes. The blocks must not overlap, each flush must give a durable position at or after its block with the block in the store, and the ring must then hold every block at its place. `bound` is the most preemptions that loom tries in one run.
    fn run(threads: u64, body: usize, bound: Option<usize>) {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = bound;
        model.check(move || {
            let io = SimIo::new(1);
            let store = store(&io);
            let ring = Arc::new(Ring::create(store.clone(), 0, 0, 1).unwrap());
            let handles: Vec<_> = (1..=threads)
                .map(|xid| {
                    let ring = ring.clone();
                    thread::spawn(move || {
                        let placed = ring.append(&mut commit(xid, body)).unwrap();
                        assert!(ring.flush_to(placed.end).unwrap() >= placed.end);
                        // When the flush returns, every block before the durable position is in the store, also the blocks of the other threads and also when another thread did the flush.
                        ring.io.touch();
                        let (read, _) = read_all(ring.store(), &ring.state(), 0);
                        assert!(read.iter().any(|b| (b.position, b.xid) == (placed.position, xid)));
                        (placed.position, xid, placed.end)
                    })
                })
                .collect();
            let mut placed: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            placed.sort_unstable();
            for w in placed.windows(2) {
                assert!(w[0].2 <= w[1].0, "{placed:?}");
            }
            ring.io.touch();
            let (read, _) = read_all(&store, &ring.state(), 0);
            let read: Vec<_> = read.iter().map(|b| (b.position, b.xid)).collect();
            let want: Vec<_> = placed.iter().map(|&(p, x, _)| (p, x)).collect();
            assert_eq!(read, want);
        });
    }

    #[test]
    fn two_threads_commit_together() {
        run(2, 100, None);
    }

    /// Blocks of more than one unit, so that a flush pads a unit that a later block does not use.
    #[test]
    fn two_threads_commit_large_blocks() {
        run(2, 5000, None);
    }

    /// With 3 threads, every interleaving takes minutes, so this test tries the runs with up to 2 preemptions.
    #[test]
    fn three_threads_commit_together() {
        run(3, 100, Some(2));
    }
}
