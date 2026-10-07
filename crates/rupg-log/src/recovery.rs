//! Recovery of the log rings after a crash (spec/11 section 11.16).
//!
//! Recovery reads each ring from its redo position to its end, cuts each ring at its first block with a dependency beyond the cut of another ring, and keeps the safe prefixes. Every acknowledged commit is in a safe prefix. The caller applies the blocks of the safe prefixes and then calls [`Recovery::finish`], which writes a checkpoint and opens the rings for new blocks.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rupg_buffer::{Checkpoint, FileStore, RingState};
use rupg_common::{Error, Hlc, Result};

use crate::block::{Block, BlockKind, Dep, UNIT};
use crate::ring::{Placed, Ring, RingReader};
use crate::rings::Log;

/// A block that recovery found, without its body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// The ring.
    pub ring: u16,
    /// The place in the ring.
    pub placed: Placed,
    /// The kind. Recovery does not keep fill blocks.
    pub kind: BlockKind,
    /// The commit timestamp.
    pub commit_ts: Hlc,
    /// The transaction id.
    pub xid: u64,
    /// The dependencies.
    pub deps: Vec<Dep>,
}

#[derive(Debug)]
struct Scanned {
    state: RingState,
    end: u64,
    cut: u64,
    blocks: Vec<Found>,
}

/// The result of the scan of every ring.
#[derive(Debug)]
pub struct Recovery {
    rings: Vec<Scanned>,
}

impl Recovery {
    /// Reads every ring of the active checkpoint of `store` and makes the cut. A ring that ends before the durable position of the checkpoint, or a block that passes its checksum and is not valid, gives SQLSTATE `XX001`.
    pub fn scan(store: &FileStore) -> Result<Recovery> {
        let mut rings = Vec::new();
        for state in store.rings() {
            // The scan needs only the headers. The checksum still covers the whole block.
            let mut reader = RingReader::new(store, &state, state.redo)?.without_bodies();
            let mut blocks = Vec::new();
            while let Some((placed, b)) = reader.next_block()? {
                blocks.push(Found {
                    ring: b.ring,
                    placed,
                    kind: b.kind,
                    commit_ts: b.commit_ts,
                    xid: b.xid,
                    deps: b.deps,
                });
            }
            let end = reader.position();
            if end < state.durable {
                return Err(Error::corrupted(format!(
                    "log ring {} ends at position {end}, before the position {} of the checkpoint",
                    state.ring, state.durable
                )));
            }
            rings.push(Scanned { state, end, cut: end, blocks });
        }
        let mut r = Recovery { rings };
        r.cut();
        Ok(r)
    }

    /// Cuts each ring at its first block with a dependency beyond the cut of its ring, until no cut moves (spec/11 section 11.16 step 3). A dependency on a ring that is not in the checkpoint is beyond its cut.
    fn cut(&mut self) {
        let index: BTreeMap<u32, usize> =
            self.rings.iter().enumerate().map(|(i, s)| (s.state.ring, i)).collect();
        loop {
            let mut moved = false;
            for i in 0..self.rings.len() {
                let cut = self.rings[i].cut;
                let first_bad = self.rings[i]
                    .blocks
                    .iter()
                    .take_while(|b| b.placed.end <= cut)
                    .find(|b| {
                        b.deps.iter().any(|d| {
                            index
                                .get(&u32::from(d.ring))
                                .is_none_or(|&q| d.position > self.rings[q].cut)
                        })
                    })
                    .map(|b| b.placed.position);
                if let Some(position) = first_bad {
                    self.rings[i].cut = position;
                    moved = true;
                }
            }
            if !moved {
                return;
            }
        }
    }

    /// The end of a ring, where its first block with a bad checksum or a wrong position is.
    pub fn end(&self, ring: u16) -> Option<u64> {
        self.ring(ring).map(|s| s.end)
    }

    /// The end of the safe prefix of a ring.
    pub fn cut_at(&self, ring: u16) -> Option<u64> {
        self.ring(ring).map(|s| s.cut)
    }

    fn ring(&self, ring: u16) -> Option<&Scanned> {
        self.rings.iter().find(|s| s.state.ring == u32::from(ring))
    }

    /// The blocks to apply, ring by ring and in ring order: each block of a safe prefix except the part blocks that no block of a safe prefix names (spec/11 section 11.16 step 5).
    pub fn safe(&self) -> Vec<&Found> {
        let safe = || {
            self.rings.iter().flat_map(|s| s.blocks.iter().take_while(|b| b.placed.end <= s.cut))
        };
        let named: BTreeSet<(u16, u64)> =
            safe().flat_map(|b| b.deps.iter().map(|d| (d.ring, d.position))).collect();
        safe()
            .filter(|b| b.kind != BlockKind::Part || named.contains(&(b.ring, b.placed.end)))
            .collect()
    }

    /// The blocks that recovery drops: the blocks after a cut and the part blocks with no commit.
    pub fn dropped(&self) -> Vec<&Found> {
        let safe: BTreeSet<(u16, u64)> =
            self.safe().iter().map(|b| (b.ring, b.placed.position)).collect();
        self.rings
            .iter()
            .flat_map(|s| &s.blocks)
            .filter(|b| !safe.contains(&(b.ring, b.placed.position)))
            .collect()
    }

    /// Reads the blocks of [`Recovery::safe`] again, with their bodies, and calls `f` with each one in the same order. Each ring is read once, from its first safe block to its last, so the cost is one more read of the log. A block that does not read again as the scan found it gives SQLSTATE `XX001`.
    pub fn for_each_safe(
        &self,
        store: &FileStore,
        mut f: impl FnMut(&Found, Block) -> Result<()>,
    ) -> Result<()> {
        let safe = self.safe();
        let mut i = 0;
        while let Some(first) = safe.get(i) {
            let ring = first.ring;
            let s = self
                .ring(ring)
                .ok_or_else(|| Error::internal(format!("recovery has no ring {ring}")))?;
            let mut reader = RingReader::new(store, &s.state, first.placed.position)?;
            while let Some(&found) = safe.get(i).filter(|b| b.ring == ring) {
                // A part block that no commit names lies between two safe blocks. The reader reads it and drops it.
                loop {
                    match reader.next_block()? {
                        Some((placed, block)) if placed == found.placed => {
                            f(found, block)?;
                            break;
                        }
                        Some((placed, _)) if placed.end <= found.placed.position => {}
                        _ => {
                            return Err(Error::corrupted(format!(
                                "the log block of ring {ring} at position {} cannot be read again",
                                found.placed.position
                            )));
                        }
                    }
                }
                i += 1;
            }
        }
        Ok(())
    }

    /// Reads the whole block of `found`, with its body. To read every safe block, [`Recovery::for_each_safe`] reads each ring once.
    pub fn read(&self, store: &FileStore, found: &Found) -> Result<Block> {
        let s = self
            .ring(found.ring)
            .ok_or_else(|| Error::internal(format!("recovery has no ring {}", found.ring)))?;
        let mut reader = RingReader::new(store, &s.state, found.placed.position)?;
        match reader.next_block()? {
            Some((placed, block)) if placed == found.placed => Ok(block),
            _ => Err(Error::corrupted(format!(
                "the log block of ring {} at position {} cannot be read again",
                found.ring, found.placed.position
            ))),
        }
    }

    /// The newest commit timestamp in the safe prefixes and the checkpoint. The HLC of the node must start above it.
    pub fn newest(&self) -> Hlc {
        let rings = self.rings.iter().map(|s| s.state.newest);
        let blocks = self.safe().into_iter().map(|b| b.commit_ts);
        rings.chain(blocks).max().unwrap_or(Hlc::ZERO)
    }

    /// Writes checkpoint `c` with the rings and opens the log. The caller must have applied the blocks of [`Recovery::safe`] to the pages first, because the checkpoint moves each redo position past them.
    ///
    /// Each ring starts again one ring size after its end. A flush that the crash stopped can leave valid blocks after the end. Their positions are one ring size lower than the positions that a later read expects in the same places, so they are never read again.
    pub fn finish(self, store: Arc<FileStore>, c: &Checkpoint) -> Result<Log> {
        let newest = self.newest();
        let rings: Vec<RingState> = self
            .rings
            .iter()
            .map(|s| {
                let start = s.end.next_multiple_of(UNIT) + s.state.size();
                RingState { redo: start, durable: start, newest, ..s.state.clone() }
            })
            .collect();
        store.checkpoint(&Checkpoint { rings: rings.clone(), ..c.clone() })?;
        Log::new(rings.iter().map(|s| Ring::open(store.clone(), s)).collect::<Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicI64, Ordering};

    use rupg_common::SqlState;
    use rupg_platform::sim::{SimIo, SimRng};
    use rupg_platform::{File, Io, OpenMode};

    use super::*;
    use crate::ring::tests::{commit, ts};

    const PATH: &str = "db";

    /// A file that fails each write, sync and size change after a number of them, as a process that dies at that point.
    #[derive(Debug)]
    struct Cut {
        inner: Arc<dyn File>,
        left: AtomicI64,
    }

    impl Cut {
        fn take(&self) -> Result<()> {
            if self.left.fetch_sub(1, Ordering::SeqCst) <= 0 {
                return Err(Error::new(SqlState::IO_ERROR, "the process died"));
            }
            Ok(())
        }
    }

    impl File for Cut {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            self.inner.read_at(offset, buf)
        }
        fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
            self.take()?;
            self.inner.write_at(offset, data)
        }
        fn sync(&self) -> Result<()> {
            self.take()?;
            self.inner.sync()
        }
        fn size(&self) -> Result<u64> {
            self.inner.size()
        }
        fn set_size(&self, size: u64) -> Result<()> {
            self.take()?;
            self.inner.set_size(size)
        }
    }

    fn cut_file(io: &SimIo, mode: OpenMode) -> Arc<Cut> {
        let inner = io.open(Path::new(PATH), mode).unwrap();
        Arc::new(Cut { inner, left: AtomicI64::new(i64::MAX) })
    }

    fn block(kind: BlockKind, xid: u64, deps: Vec<Dep>) -> Block {
        Block { kind, deps, ..commit(xid, 24 + (xid % 7) as usize * 40) }
    }

    /// What the test wrote: each block by ring and end, with its kind and transaction.
    #[derive(Debug, Default)]
    struct Model {
        blocks: BTreeMap<(u16, u64), (BlockKind, u64)>,
        parts: BTreeMap<u64, usize>,
        acked: BTreeSet<u64>,
        aborted: BTreeSet<u64>,
    }

    /// One transaction on `ring`: some part blocks, then a commit or an abort. A commit can depend on the last block of another ring.
    fn transaction(
        log: &Log,
        model: &mut Model,
        rng: &mut SimRng,
        ring: u16,
        xid: u64,
    ) -> Result<()> {
        let mut deps = Vec::new();
        let parts = if rng.below(4) == 0 { 1 + rng.below(3) as usize } else { 0 };
        for _ in 0..parts {
            let p = log.append(ring, &mut block(BlockKind::Part, xid, Vec::new()))?;
            model.blocks.insert((ring, p.end), (BlockKind::Part, xid));
            deps.push(Dep { ring, position: p.end });
        }
        if parts > 0 && rng.below(5) == 0 {
            let a = log.append(ring, &mut block(BlockKind::Abort, xid, Vec::new()))?;
            model.blocks.insert((ring, a.end), (BlockKind::Abort, xid));
            model.aborted.insert(xid);
            return Ok(());
        }
        let other = rng.below(log.rings().len() as u64) as u16;
        if other != ring && rng.below(2) == 0 {
            let end = log.ring(other)?.end();
            if model.blocks.contains_key(&(other, end)) {
                deps.push(Dep { ring: other, position: end });
            }
        }
        let c = log.append(ring, &mut block(BlockKind::Commit, xid, deps))?;
        model.blocks.insert((ring, c.end), (BlockKind::Commit, xid));
        model.parts.insert(xid, parts);
        if rng.below(3) == 0 {
            log.wait_safe(ring, c.end)?;
            model.acked.insert(xid);
        } else if rng.below(4) == 0 {
            log.flush(ring, c.end)?;
        }
        Ok(())
    }

    /// Checks the scan against the model and gives the transactions that recovery keeps.
    fn check(rec: &Recovery, model: &Model, applied: &BTreeSet<u64>) -> BTreeSet<u64> {
        let safe = rec.safe();
        let mut kept = BTreeSet::new();
        let mut parts: BTreeMap<u64, usize> = BTreeMap::new();
        for b in &safe {
            let &(kind, xid) = model.blocks.get(&(b.ring, b.placed.end)).unwrap_or_else(|| {
                panic!("recovery found a block that the test did not write: {b:?}")
            });
            assert_eq!((kind, xid), (b.kind, b.xid));
            match kind {
                BlockKind::Commit => {
                    kept.insert(xid);
                }
                BlockKind::Part => *parts.entry(xid).or_default() += 1,
                _ => {}
            }
        }
        for b in &safe {
            if b.kind != BlockKind::Commit {
                continue;
            }
            // Each block that a kept commit waited for is kept too, or was applied before.
            for d in &b.deps {
                let (kind, xid) = model.blocks[&(d.ring, d.position)];
                if kind == BlockKind::Commit {
                    assert!(kept.contains(&xid) || applied.contains(&xid), "{} needs {xid}", b.xid);
                }
            }
            assert_eq!(
                parts.remove(&b.xid).unwrap_or(0),
                model.parts[&b.xid],
                "parts of {}",
                b.xid
            );
        }
        assert!(parts.is_empty(), "part blocks with no commit: {parts:?}");
        assert!(kept.is_disjoint(&model.aborted));
        for xid in &model.acked {
            assert!(
                kept.contains(xid) || applied.contains(xid),
                "acknowledged commit {xid} is lost"
            );
        }
        kept
    }

    /// The commits of the model that a checkpoint moved before the redo position of their ring.
    fn applied_by_checkpoint(store: &FileStore, model: &Model) -> BTreeSet<u64> {
        let redo: BTreeMap<u16, u64> =
            store.rings().iter().map(|r| (r.ring as u16, r.redo)).collect();
        model
            .blocks
            .iter()
            .filter(|((ring, end), (kind, _))| *kind == BlockKind::Commit && *end <= redo[ring])
            .map(|(_, &(_, xid))| xid)
            .collect()
    }

    /// A checkpoint that moves each redo position to the safe position, but not past a part block of a transaction with no safe commit.
    fn checkpoint(log: &Log, store: &FileStore, model: &Model, n: u64) -> Result<()> {
        let mut rings = Vec::new();
        for ring in log.rings() {
            let r = ring.number();
            let safe = log.safe(r);
            let open_part = model
                .blocks
                .range((r, 0)..=(r, u64::MAX))
                .filter(|((_, end), (kind, xid))| {
                    *kind == BlockKind::Part
                        && *end > ring.state().redo
                        && !model.blocks.iter().any(|((q, e), (k, x))| {
                            x == xid && *k != BlockKind::Part && *q == r && *e <= safe
                        })
                })
                .map(|((_, end), _)| *end)
                .next();
            let redo = open_part.map_or(safe, |end| {
                // The start of the part block is at or before its end minus the smallest block.
                let start =
                    model.blocks.range((r, 0)..(r, end)).next_back().map_or(0, |((_, e), _)| *e);
                start.min(safe)
            });
            ring.set_redo(redo.max(ring.state().redo))?;
            rings.push(ring.state());
        }
        store.checkpoint(&Checkpoint { timestamp: ts(n), rings, ..Checkpoint::default() })
    }

    #[test]
    fn a_crash_loses_no_acknowledged_commit() {
        let mut rounds = 0;
        for seed in 0..12u64 {
            let io = SimIo::new(seed);
            let mut rng = SimRng::new(seed + 100);
            let file = cut_file(&io, OpenMode::CreateNew);
            let mut store = Arc::new(FileStore::create(file.clone(), [9; 16], ts(0)).unwrap());
            let rings: Vec<Ring> =
                (0..3).map(|n| Ring::create(store.clone(), n, n, 1).unwrap()).collect();
            let states: Vec<RingState> = rings.iter().map(Ring::state).collect();
            store
                .checkpoint(&Checkpoint {
                    timestamp: ts(1),
                    rings: states,
                    ..Checkpoint::default()
                })
                .unwrap();
            let mut log = Log::new(rings).unwrap();
            let mut file = file;
            let mut applied = BTreeSet::new();
            let mut xid = 0;
            for round in 0..6u64 {
                rounds += 1;
                let mut model = Model::default();
                file.left.store(rng.below(80) as i64, Ordering::SeqCst);
                for _ in 0..60 {
                    xid += 1;
                    let ring = rng.below(3) as u16;
                    let step = if rng.below(10) == 0 {
                        checkpoint(&log, &store, &model, 10 + round * 100 + xid)
                    } else {
                        transaction(&log, &mut model, &mut rng, ring, xid)
                    };
                    if step.is_err() {
                        break;
                    }
                }
                drop(log);
                drop(store);
                io.crash_random().unwrap();

                file = cut_file(&io, OpenMode::ReadWrite);
                store = Arc::new(FileStore::open(file.clone()).unwrap());
                let before = applied_by_checkpoint(&store, &model);
                applied.extend(before);
                let rec = Recovery::scan(&store).unwrap();
                for ring in 0..3u16 {
                    assert!(rec.cut_at(ring).unwrap() <= rec.end(ring).unwrap());
                }
                let kept = check(&rec, &model, &applied);
                for b in rec.safe() {
                    let full = rec.read(&store, b).unwrap();
                    assert_eq!((full.xid, full.kind), (b.xid, b.kind));
                }
                assert!(
                    rec.newest()
                        >= rec.safe().iter().map(|b| b.commit_ts).max().unwrap_or(Hlc::ZERO)
                );
                applied.extend(kept);
                let c =
                    Checkpoint { timestamp: ts(20 + round * 100 + xid), ..Checkpoint::default() };
                log = rec.finish(store.clone(), &c).unwrap();
                // Each ring starts after its old end, and a scan now finds nothing.
                let again = Recovery::scan(&store).unwrap();
                assert!(again.safe().is_empty() && again.dropped().is_empty());
            }
        }
        assert_eq!(rounds, 72);
    }

    /// A flush that the crash stopped can leave a valid block after a hole. Recovery ends the ring at the hole, and the block must not come back when new blocks reach its place.
    #[test]
    fn a_stale_tail_is_not_read_again() {
        let io = SimIo::new(60);
        let store = Arc::new(
            FileStore::create(cut_file(&io, OpenMode::CreateNew), [9; 16], ts(0)).unwrap(),
        );
        let ring = Ring::create(store.clone(), 0, 0, 1).unwrap();
        let state = ring.state();
        store
            .checkpoint(&Checkpoint {
                timestamp: ts(1),
                rings: vec![state.clone()],
                ..Checkpoint::default()
            })
            .unwrap();
        let log = Log::new(vec![ring]).unwrap();
        let a = log.commit(0, &mut block(BlockKind::Commit, 1, Vec::new())).unwrap();
        let end = log.ring(0).unwrap().durable();
        assert_eq!(end, a.end.next_multiple_of(UNIT));
        drop(log);

        // The unit at `end` is lost and the unit after it reached the disk.
        let stale =
            Block { ring: 0, position: end + UNIT, ..block(BlockKind::Commit, 2, Vec::new()) };
        let mut bytes = Vec::new();
        stale.encode(&mut bytes).unwrap();
        store.write_ring(state.extents[0], end + UNIT, &bytes).unwrap();
        store.sync_rings().unwrap();

        let rec = Recovery::scan(&store).unwrap();
        assert_eq!(rec.end(0), Some(end));
        assert_eq!(rec.safe().iter().map(|b| b.xid).collect::<Vec<_>>(), [1]);
        let log = rec
            .finish(store.clone(), &Checkpoint { timestamp: ts(2), ..Checkpoint::default() })
            .unwrap();
        let start = end + state.size();
        assert_eq!(log.ring(0).unwrap().end(), start);
        let d = log.commit(0, &mut block(BlockKind::Commit, 3, Vec::new())).unwrap();
        assert_eq!(d.position, start);
        drop(log);

        let rec = Recovery::scan(&store).unwrap();
        assert_eq!(rec.safe().iter().map(|b| b.xid).collect::<Vec<_>>(), [3]);
        assert_eq!(rec.end(0), Some(start + UNIT));
    }

    #[test]
    fn the_cut_follows_dependencies() {
        let io = SimIo::new(50);
        let file = cut_file(&io, OpenMode::CreateNew);
        let store = Arc::new(FileStore::create(file.clone(), [9; 16], ts(0)).unwrap());
        let rings: Vec<Ring> =
            (0..3).map(|n| Ring::create(store.clone(), n, n, 1).unwrap()).collect();
        let states: Vec<RingState> = rings.iter().map(Ring::state).collect();
        store
            .checkpoint(&Checkpoint { timestamp: ts(1), rings: states, ..Checkpoint::default() })
            .unwrap();
        let log = Log::new(rings).unwrap();

        // Ring 2 is never flushed. Ring 1 waits for it, and ring 0 waits for ring 1.
        let c = log.append(2, &mut block(BlockKind::Commit, 1, Vec::new())).unwrap();
        let b0 = log.append(1, &mut block(BlockKind::Commit, 2, Vec::new())).unwrap();
        let b = log
            .append(1, &mut block(BlockKind::Commit, 3, vec![Dep { ring: 2, position: c.end }]))
            .unwrap();
        let a0 = log.append(0, &mut block(BlockKind::Commit, 4, Vec::new())).unwrap();
        let a = log
            .append(0, &mut block(BlockKind::Commit, 5, vec![Dep { ring: 1, position: b.end }]))
            .unwrap();
        let p = log.append(0, &mut block(BlockKind::Part, 6, Vec::new())).unwrap();
        log.flush(0, p.end).unwrap();
        log.flush(1, b.end).unwrap();
        drop(log);
        io.crash_random().unwrap();

        let store = Arc::new(FileStore::open(cut_file(&io, OpenMode::ReadWrite)).unwrap());
        let rec = Recovery::scan(&store).unwrap();
        assert_eq!(rec.end(2), Some(0));
        assert_eq!((rec.cut_at(1), rec.cut_at(0)), (Some(b.position), Some(a.position)));
        assert!(rec.end(0).unwrap() >= p.end);
        let xids = |v: Vec<&Found>| v.iter().map(|b| b.xid).collect::<Vec<_>>();
        assert_eq!(xids(rec.safe()), [4, 2]);
        assert_eq!(xids(rec.dropped()), [5, 6, 3]);
        assert_eq!((a0.end, b0.end), (a.position, b.position));
        assert_eq!(rec.end(7), None);
    }
}
