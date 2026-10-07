//! `FileStore`, the store of a database file (spec/08 sections 8.5, 8.6 and 8.9).
//!
//! A page is never written over its durable copy. Each write goes to the next free page of the current write arena, and the page table entry in memory moves to the new copy. The old copy goes on the pending free list. A checkpoint writes the page table nodes that changed, the pending free list and the free space map, syncs, writes the inactive header slot and syncs again. A superseded page becomes free at the second checkpoint after its write, because the old slot still names it until the next checkpoint writes over that slot.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rupg_common::{Error, Hlc, Result, SqlState};
use rupg_file::{
    ARENA_PAGES, ArenaKind, ArenaMap, BLOCK_SIZE, Block, FIRST_ARENA_PAGE, FREE_LIST_PER_PAGE,
    FSM_ARENAS_PER_PAGE, Features, FileId, FreeListPage, FsmPage, Identity, PAGE_SIZE, PT_FANOUT,
    PT_MAX_LEVELS, PT_MIN_LEVELS, Page, PageHeader, PageId, PageKind, PtEntry, PtNode, Root,
    SLOT_A_PAGE, SLOT_B_PAGE, Slot, SlotName, arena_of, arena_start, choose_slot, pt_capacity,
    pt_levels, seal, stored_checksum, verify,
};
use rupg_platform::File;

use crate::Store;

/// The time and the roots that the caller gives to a checkpoint. The page table, the free space map and the pending free list come from the store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Checkpoint {
    /// The time of the checkpoint. It is the page timestamp of each metadata page that the checkpoint writes.
    pub timestamp: Hlc,
    /// The root of the catalog.
    pub catalog: Root,
    /// The log ring directory.
    pub ring_directory: Root,
    /// The root of the shard map.
    pub shard_map: Root,
    /// True when the checkpoint ends a clean shutdown.
    pub clean_shutdown: bool,
}

/// Counts of a [`FileStore`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileStats {
    /// The length of the file in pages.
    pub file_pages: u64,
    /// The next logical page number to give out.
    pub next_page: u64,
    /// Physical pages in use, pending free pages included.
    pub used_pages: u64,
    /// Physical pages on the pending free lists of this interval and of the previous one.
    pub pending_pages: u64,
    /// Pages read.
    pub reads: u64,
    /// Pages written, metadata pages not included.
    pub writes: u64,
    /// The generation of the active header slot.
    pub generation: u64,
}

/// The store of a database file. It implements [`Store`] for the buffer pool.
///
/// An I/O error during a sync or a checkpoint stops the store. After that each call fails, and the caller must open the file again so that recovery runs (spec/21 section 21.9.1).
#[derive(Debug)]
pub struct FileStore {
    file: Arc<dyn File>,
    inner: RwLock<Inner>,
    stopped: AtomicBool,
    reads: AtomicU64,
    writes: AtomicU64,
}

/// A page table node in memory.
#[derive(Debug)]
struct Node {
    page: Box<Page>,
    /// The entry of the durable copy, or the empty entry for a node that was never written.
    entry: PtEntry,
    dirty: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct Arena {
    kind: ArenaKind,
    map: ArenaMap,
}

#[derive(Debug)]
struct Inner {
    /// The active slot and its content.
    active: SlotName,
    slot: Slot,
    read_only: bool,
    levels: u8,
    /// The page table, by level and index. The node of level `l` with index `i` covers the logical pages from `i * span(l + 1)`.
    nodes: BTreeMap<(u8, u64), Node>,
    arenas: Vec<Arena>,
    /// The write arena and the first page offset to try in it.
    cursor: Option<(usize, u16)>,
    next_page: u64,
    /// Logical page numbers that can be given out again.
    free: BTreeSet<u64>,
    /// Logical page numbers released in this interval. They can be given out after the next checkpoint.
    freed: BTreeSet<u64>,
    /// Physical pages superseded in this interval.
    pending_now: Vec<u64>,
    /// The pending free list of the active slot. These pages become free at the next checkpoint.
    pending_prev: Vec<u64>,
    /// The free space map pages and the pending free list pages of the active slot.
    fsm_at: Vec<u64>,
    list_at: Vec<u64>,
}

/// The logical pages that one entry of a node at `level` covers.
fn span(level: u8) -> u64 {
    pt_capacity(level)
}

fn offset(physical: u64) -> u64 {
    physical * PAGE_SIZE as u64
}

fn not_allocated(page: PageId) -> Error {
    Error::internal(format!("logical page {page} is not allocated"))
}

fn stopped() -> Error {
    Error::new(SqlState::IO_ERROR, "the file store stopped after an earlier I/O error")
        .with_hint("Open the database again so that recovery runs.")
}

/// Sets the physical page number and the timestamp in the header of a metadata page and seals it. Gives the checksum.
fn stamp(page: &mut Page, physical: u64, timestamp: Hlc) -> Result<u64> {
    let mut header = PageHeader::read(page)?;
    header.page = PageId(physical);
    header.timestamp = timestamp;
    header.write(page);
    Ok(seal(page))
}

fn entry(physical: u64, timestamp: Hlc) -> Result<PtEntry> {
    PtEntry::new(physical, timestamp)
        .ok_or_else(|| Error::internal(format!("physical page {physical} is out of range")))
}

/// How [`read_meta`] checks a metadata page.
enum Expect {
    /// The page that a slot names, with the checksum from the slot.
    Root(u64),
    /// A page that an entry names, with the write tag from the entry.
    Tag(u16),
}

/// Reads a metadata page at `physical` and checks it.
fn read_meta(file: &dyn File, physical: u64, kind: PageKind, expect: Expect) -> Result<Box<Page>> {
    let mut page = Box::new([0u8; PAGE_SIZE]);
    file.read_at(offset(physical), &mut page[..])?;
    let tag = match expect {
        Expect::Root(sum) => {
            if stored_checksum(&page) != sum {
                return Err(Error::corrupted(format!(
                    "the {} page {physical} does not match the checksum in the header slot",
                    kind.name()
                )));
            }
            None
        }
        Expect::Tag(tag) => Some(tag),
    };
    let header = verify(&page, physical, PageId(physical), tag)?;
    if header.kind != kind {
        return Err(Error::corrupted(format!(
            "page {physical} has the kind {} and not {}",
            header.kind,
            kind.name()
        )));
    }
    Ok(page)
}

fn read_block(file: &dyn File, page: u64) -> Result<Block> {
    let mut block = [0u8; BLOCK_SIZE];
    file.read_at(offset(page), &mut block)?;
    Ok(block)
}

impl Inner {
    fn file_pages(&self) -> u64 {
        FIRST_ARENA_PAGE + self.arenas.len() as u64 * ARENA_PAGES
    }

    fn root_key(&self) -> (u8, u64) {
        (self.levels - 1, 0)
    }

    fn new_node(level: u8, index: u64) -> Node {
        let mut page = Box::new([0u8; PAGE_SIZE]);
        PtNode::init(&mut page, 0, level, PageId(index * span(level + 1)));
        Node { page, entry: PtEntry::EMPTY, dirty: true }
    }

    /// The leaf entry of a logical page.
    fn entry(&self, page: u64) -> PtEntry {
        match self.nodes.get(&(0, page / span(1))) {
            Some(node) => PtNode::entry(&node.page, (page % span(1)) as u16),
            None => PtEntry::EMPTY,
        }
    }

    /// Sets the leaf entry of a logical page. It makes the nodes on the path if they do not exist.
    fn set_entry(&mut self, page: u64, value: PtEntry) -> Result<()> {
        for level in 1..self.levels {
            self.nodes
                .entry((level, page / span(level + 1)))
                .or_insert_with(|| Inner::new_node(level, page / span(level + 1)));
        }
        let leaf = self
            .nodes
            .entry((0, page / span(1)))
            .or_insert_with(|| Inner::new_node(0, page / span(1)));
        PtNode::set_entry(&mut leaf.page, (page % span(1)) as u16, value)?;
        leaf.dirty = true;
        Ok(())
    }

    fn is_allocated(&self, page: u64) -> bool {
        page >= 1
            && page < self.next_page
            && !self.free.contains(&page)
            && !self.freed.contains(&page)
    }

    fn writable(&self) -> Result<()> {
        if self.read_only {
            return Err(Error::new(
                SqlState::READ_ONLY_SQL_TRANSACTION,
                "the file is open read-only, because it uses a feature that this rupg does not know",
            ));
        }
        Ok(())
    }

    /// Takes a free physical page from the write arena. The file grows when no arena has a free page.
    fn take_page(&mut self, file: &dyn File) -> Result<u64> {
        loop {
            if let Some((a, from)) = self.cursor {
                let arena = &mut self.arenas[a];
                if let Some(off) = arena.map.allocate_page(from) {
                    arena.kind = ArenaKind::Write;
                    self.cursor = Some((a, off + 1));
                    return Ok(arena_start(a as u64) + u64::from(off));
                }
                self.cursor = None;
            }
            let next = self.arenas.iter().position(|a| a.kind == ArenaKind::Free).or_else(|| {
                self.arenas
                    .iter()
                    .position(|a| a.kind == ArenaKind::Write && a.map.free_pages() > 0)
            });
            match next {
                Some(a) => self.cursor = Some((a, 0)),
                None => self.grow(file)?,
            }
        }
    }

    /// Grows the file by the larger of one arena and one eighth of the arenas (spec/08 section 8.9.3).
    fn grow(&mut self, file: &dyn File) -> Result<()> {
        let add = (self.arenas.len() / 8).max(1);
        let pages = FIRST_ARENA_PAGE + (self.arenas.len() + add) as u64 * ARENA_PAGES;
        file.set_size(offset(pages))?;
        self.arenas.resize(self.arenas.len() + add, Arena::default());
        Ok(())
    }

    /// Frees a physical page in `arenas`. An arena with no page in use becomes free.
    fn release(arenas: &mut [Arena], physical: u64) -> Result<()> {
        let a = arena_of(physical).filter(|&a| (a as usize) < arenas.len()).ok_or_else(|| {
            Error::internal(format!("physical page {physical} is not in an arena"))
        })?;
        let arena = &mut arenas[a as usize];
        arena.map.release((physical - arena_start(a)) as u16, 1)?;
        if arena.map.free_pages() as u64 == ARENA_PAGES {
            arena.kind = ArenaKind::Free;
        }
        Ok(())
    }

    fn allocate(&mut self) -> Result<PageId> {
        if let Some(page) = self.free.pop_first() {
            return Ok(PageId(page));
        }
        let page = self.next_page;
        if page >= pt_capacity(self.levels) {
            if self.levels >= PT_MAX_LEVELS {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_RESOURCES,
                    "the file has no free logical page number",
                ));
            }
            // A new root over the old one. The old root is entry 0 of the new root.
            let old = self.nodes[&self.root_key()].entry;
            let mut root = Inner::new_node(self.levels, 0);
            PtNode::set_entry(&mut root.page, 0, old)?;
            self.nodes.insert((self.levels, 0), root);
            self.levels += 1;
        }
        self.next_page += 1;
        Ok(PageId(page))
    }

    fn free_page(&mut self, page: PageId) -> Result<()> {
        if !self.is_allocated(page.0) {
            return Err(not_allocated(page));
        }
        if let Some(old) = self.entry(page.0).physical() {
            self.pending_now.push(old);
            self.set_entry(page.0, PtEntry::EMPTY)?;
        }
        self.freed.insert(page.0);
        Ok(())
    }

    fn write(&mut self, file: &dyn File, page: PageId, from: &Page) -> Result<()> {
        if !self.is_allocated(page.0) {
            return Err(not_allocated(page));
        }
        let header = PageHeader::read(from).map_err(|e| {
            Error::internal(format!("the page to write has no valid header: {}", e.message()))
        })?;
        if header.page != page {
            return Err(Error::internal(format!(
                "the page header names logical page {} and not {page}",
                header.page
            )));
        }
        let mut copy = Box::new(*from);
        seal(&mut copy);
        let at = self.take_page(file)?;
        let value = entry(at, header.timestamp)?;
        if let Err(e) = file.write_at(offset(at), &copy[..]) {
            Inner::release(&mut self.arenas, at)?;
            return Err(e);
        }
        if let Some(old) = self.entry(page.0).physical() {
            self.pending_now.push(old);
        }
        self.set_entry(page.0, value)
    }

    /// The checkpoint of spec/08 section 8.6.2, steps 3 to 7. The caller has written the dirty pages.
    fn checkpoint(&mut self, file: &dyn File, c: &Checkpoint) -> Result<()> {
        let ts = c.timestamp;
        let top = self.levels - 1;
        // The parent of each dirty node changes too, up to the root.
        for level in 0..top {
            let parents: Vec<u64> = self
                .nodes
                .range((level, 0)..(level + 1, 0))
                .filter(|(_, n)| n.dirty)
                .map(|(&(_, i), _)| i / PT_FANOUT as u64)
                .collect();
            for p in parents {
                self.nodes
                    .get_mut(&(level + 1, p))
                    .ok_or_else(|| Error::internal("a page table node has no parent"))?
                    .dirty = true;
            }
        }
        let dirty: Vec<(u8, u64)> =
            self.nodes.iter().filter(|(_, n)| n.dirty).map(|(&k, _)| k).collect();

        // The pages that the active slot uses and the new slot does not. They go on the new pending free list.
        let mut pending = std::mem::take(&mut self.pending_now);
        pending.extend(dirty.iter().filter_map(|k| self.nodes[k].entry.physical()));
        pending.append(&mut self.fsm_at);
        pending.append(&mut self.list_at);
        pending.sort_unstable();

        // Places for the metadata pages. The free space map comes last, because the growth of the file can add map pages.
        let node_at = dirty.iter().map(|_| self.take_page(file)).collect::<Result<Vec<_>>>()?;
        let list_at = (0..pending.len().div_ceil(FREE_LIST_PER_PAGE))
            .map(|_| self.take_page(file))
            .collect::<Result<Vec<_>>>()?;
        let mut fsm_at = Vec::new();
        while fsm_at.len() < self.arenas.len().div_ceil(FSM_ARENAS_PER_PAGE) {
            fsm_at.push(self.take_page(file)?);
        }

        // The page table from the leaves to the root. A node gets the entries of its children before it is written.
        for (&key, &at) in dirty.iter().zip(&node_at) {
            let node = self
                .nodes
                .get_mut(&key)
                .ok_or_else(|| Error::internal("a page table node is missing"))?;
            stamp(&mut node.page, at, ts)?;
            file.write_at(offset(at), &node.page[..])?;
            node.entry = entry(at, ts)?;
            node.dirty = false;
            let value = node.entry;
            if key.0 < top {
                let parent = self
                    .nodes
                    .get_mut(&(key.0 + 1, key.1 / PT_FANOUT as u64))
                    .ok_or_else(|| Error::internal("a page table node has no parent"))?;
                PtNode::set_entry(&mut parent.page, (key.1 % PT_FANOUT as u64) as u16, value)?;
            }
        }

        let mut page = Box::new([0u8; PAGE_SIZE]);
        let mut list_sum = 0;
        for (i, chunk) in pending.chunks(FREE_LIST_PER_PAGE).enumerate() {
            let next = match list_at.get(i + 1) {
                Some(&n) => entry(n, ts)?,
                None => PtEntry::EMPTY,
            };
            FreeListPage::init(&mut page, list_at[i], chunk, next)?;
            let sum = stamp(&mut page, list_at[i], ts)?;
            if i == 0 {
                list_sum = sum;
            }
            file.write_at(offset(list_at[i]), &page[..])?;
        }

        // The map that the new slot names shows the pages of the old pending free list as free, because they are free when that slot is durable.
        let mut after = self.arenas.clone();
        for &p in &self.pending_prev {
            Inner::release(&mut after, p)?;
        }
        let mut fsm_sum = 0;
        for (i, &at) in fsm_at.iter().enumerate() {
            let first = i * FSM_ARENAS_PER_PAGE;
            let count = (after.len() - first).min(FSM_ARENAS_PER_PAGE);
            let next = match fsm_at.get(i + 1) {
                Some(&n) => entry(n, ts)?,
                None => PtEntry::EMPTY,
            };
            FsmPage::init(&mut page, at, first as u64, count as u16, next);
            for (r, arena) in after[first..first + count].iter().enumerate() {
                FsmPage::set_record(&mut page, r as u16, arena.kind, &arena.map)?;
            }
            let sum = stamp(&mut page, at, ts)?;
            if i == 0 {
                fsm_sum = sum;
            }
            file.write_at(offset(at), &page[..])?;
        }
        file.sync()?;

        let root = &self.nodes[&self.root_key()];
        let slot = Slot {
            generation: self.slot.generation + 1,
            file_id: self.slot.file_id,
            features: self.slot.features,
            checkpoint: ts,
            file_pages: self.file_pages(),
            page_table: Root {
                page: root.entry.physical().unwrap_or(0),
                checksum: stored_checksum(&root.page),
            },
            catalog: c.catalog,
            ring_directory: c.ring_directory,
            free_space: Root { page: fsm_at[0], checksum: fsm_sum },
            shard_map: c.shard_map,
            pending_free: Root { page: list_at.first().copied().unwrap_or(0), checksum: list_sum },
            next_page: self.next_page,
            key_check: self.slot.key_check,
            clean_shutdown: c.clean_shutdown,
        };
        let mut block = [0u8; BLOCK_SIZE];
        slot.encode(&mut block);
        let name = self.active.other();
        file.write_at(offset(name.page()), &block)?;
        file.sync()?;

        // Step 7. The old pending free list is free, and the numbers released in the interval can be given out.
        self.arenas = after;
        self.pending_prev = pending;
        self.fsm_at = fsm_at;
        self.list_at = list_at;
        self.free.append(&mut self.freed);
        self.active = name;
        self.slot = slot;
        Ok(())
    }

    /// Checks that the arenas mark exactly the pages in use, and that no page has two uses.
    fn check(&self) -> Result<()> {
        let mut used = HashSet::new();
        let mut add = |p: u64, what: &str| {
            if used.insert(p) {
                Ok(())
            } else {
                Err(Error::internal(format!("physical page {p} has two uses, one is {what}")))
            }
        };
        for (&(level, index), node) in &self.nodes {
            if let Some(p) = node.entry.physical() {
                add(p, "a page table node")?;
            }
            if level == 0 {
                for s in 0..PT_FANOUT as u16 {
                    if let Some(p) = PtNode::entry(&node.page, s).physical() {
                        add(p, &format!("logical page {}", index * span(1) + u64::from(s)))?;
                    }
                }
            }
        }
        for &p in &self.fsm_at {
            add(p, "a free space map page")?;
        }
        for &p in &self.list_at {
            add(p, "a pending free list page")?;
        }
        for &p in self.pending_now.iter().chain(&self.pending_prev) {
            add(p, "a pending free page")?;
        }
        let mut marked = 0;
        for (a, arena) in self.arenas.iter().enumerate() {
            for off in 0..ARENA_PAGES as u16 {
                let p = arena_start(a as u64) + u64::from(off);
                match (arena.map.is_used(off), used.contains(&p)) {
                    (true, true) => marked += 1,
                    (true, false) => {
                        return Err(Error::internal(format!(
                            "physical page {p} is marked in use but has no use"
                        )));
                    }
                    (false, true) => {
                        return Err(Error::internal(format!(
                            "physical page {p} is in use but marked free"
                        )));
                    }
                    (false, false) => {}
                }
            }
        }
        if marked != used.len() {
            return Err(Error::internal("a page in use is outside the arenas"));
        }
        Ok(())
    }
}

impl FileStore {
    /// Makes a new database in `file`, which must be empty. It writes the identity block and a first checkpoint with generation 1 in slot A. The caller syncs the directory after this, so that the name of the file is durable.
    pub fn create(file: Arc<dyn File>, file_id: FileId, now: Hlc) -> Result<FileStore> {
        if file.size()? != 0 {
            return Err(Error::new(
                SqlState::DUPLICATE_FILE,
                "the file for a new database is not empty",
            ));
        }
        let mut block = [0u8; BLOCK_SIZE];
        Identity::new(file_id, now, concat!("rupg ", env!("CARGO_PKG_VERSION"))).encode(&mut block);
        file.write_at(0, &block)?;
        file.set_size(offset(FIRST_ARENA_PAGE))?;
        let levels = PT_MIN_LEVELS;
        let mut nodes = BTreeMap::new();
        nodes.insert((levels - 1, 0), Inner::new_node(levels - 1, 0));
        let inner = Inner {
            // The first checkpoint writes the other slot, A.
            active: SlotName::B,
            slot: Slot { file_id, ..Slot::default() },
            read_only: false,
            levels,
            nodes,
            arenas: Vec::new(),
            cursor: None,
            next_page: 1,
            free: BTreeSet::new(),
            freed: BTreeSet::new(),
            pending_now: Vec::new(),
            pending_prev: Vec::new(),
            fsm_at: Vec::new(),
            list_at: Vec::new(),
        };
        let store = FileStore::with(file, inner);
        store.checkpoint(&Checkpoint { timestamp: now, ..Checkpoint::default() })?;
        Ok(store)
    }

    /// Opens a database. It chooses the active slot by the rule of spec/08 section 8.2.2, then reads and checks the page table, the free space map and the pending free list that the slot names. A logical page number below the next number that has no copy is free again.
    pub fn open(file: Arc<dyn File>) -> Result<FileStore> {
        let identity = match Identity::decode(&read_block(&*file, 0)?) {
            Ok(id) => Some(id.file_id),
            Err(e) if e.state() == SqlState::DATA_CORRUPTED => None,
            Err(e) => return Err(e),
        };
        let a = read_block(&*file, SLOT_A_PAGE)?;
        let b = read_block(&*file, SLOT_B_PAGE)?;
        let choice = choose_slot(identity.as_ref(), &a, &b)?;
        let slot = choice.slot;
        let read_only = slot.features.access(&Features::KNOWN)? != rupg_file::Access::ReadWrite;
        let levels = pt_levels(slot.next_page);

        // The page table. Each node is checked against the entry of its parent.
        let mut nodes = BTreeMap::new();
        let root = read_meta(
            &*file,
            slot.page_table.page,
            PageKind::PageTable,
            Expect::Root(slot.page_table.checksum),
        )?;
        let root_entry = entry(slot.page_table.page, PageHeader::read(&root)?.timestamp)?;
        let mut todo = vec![(levels - 1, 0u64, root, root_entry)];
        while let Some((level, index, page, at)) = todo.pop() {
            if PtNode::level(&page) != level
                || PtNode::first(&page) != PageId(index * span(level + 1))
            {
                return Err(Error::corrupted(format!(
                    "the page table node at physical page {} is not at its place in the tree",
                    at.physical().unwrap_or(0)
                )));
            }
            if level > 0 {
                for s in 0..PT_FANOUT as u16 {
                    let child = PtNode::entry(&page, s);
                    if let Some(p) = child.physical() {
                        let node =
                            read_meta(&*file, p, PageKind::PageTable, Expect::Tag(child.tag()))?;
                        todo.push((
                            level - 1,
                            index * PT_FANOUT as u64 + u64::from(s),
                            node,
                            child,
                        ));
                    }
                }
            }
            nodes.insert((level, index), Node { page, entry: at, dirty: false });
        }

        // The free space map.
        let count = slot.file_pages.saturating_sub(FIRST_ARENA_PAGE) / ARENA_PAGES;
        let mut arenas = Vec::new();
        let mut fsm_at = Vec::new();
        let mut at = slot.free_space.page;
        let mut expect = Expect::Root(slot.free_space.checksum);
        while at != 0 {
            let page = read_meta(&*file, at, PageKind::FreeSpace, expect)?;
            if FsmPage::first(&page) != arenas.len() as u64 {
                return Err(Error::corrupted(format!(
                    "the free space map page {at} is not at its place in the chain"
                )));
            }
            for i in 0..FsmPage::count(&page) {
                let (entry, map) = FsmPage::record(&page, i)?;
                arenas.push(Arena { kind: entry.kind, map });
            }
            fsm_at.push(at);
            let next = FsmPage::next(&page);
            at = next.physical().unwrap_or(0);
            expect = Expect::Tag(next.tag());
        }
        if arenas.len() as u64 != count {
            return Err(Error::corrupted(format!(
                "the free space map has {} arenas, and the header slot gives {count}",
                arenas.len()
            )));
        }

        // The pending free list. Its pages stay in use until the next checkpoint.
        let mut pending_prev = Vec::new();
        let mut list_at = Vec::new();
        let mut at = slot.pending_free.page;
        let mut expect = Expect::Root(slot.pending_free.checksum);
        while at != 0 {
            let page = read_meta(&*file, at, PageKind::FreeList, expect)?;
            pending_prev.extend(FreeListPage::pages(&page)?);
            list_at.push(at);
            let next = FreeListPage::next(&page);
            at = next.physical().unwrap_or(0);
            expect = Expect::Tag(next.tag());
        }

        let mut inner = Inner {
            active: choice.name,
            read_only,
            levels,
            nodes,
            arenas,
            cursor: None,
            next_page: slot.next_page,
            free: BTreeSet::new(),
            freed: BTreeSet::new(),
            pending_now: Vec::new(),
            pending_prev,
            fsm_at,
            list_at,
            slot,
        };
        inner.free = (1..inner.next_page).filter(|&p| inner.entry(p) == PtEntry::EMPTY).collect();
        inner.check().map_err(|e| {
            Error::corrupted(format!(
                "the free space map does not match the page table: {}",
                e.message()
            ))
        })?;
        Ok(FileStore::with(file, inner))
    }

    fn with(file: Arc<dyn File>, inner: Inner) -> FileStore {
        FileStore {
            file,
            inner: RwLock::new(inner),
            stopped: AtomicBool::new(false),
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
        }
    }

    fn read_lock(&self) -> Result<RwLockReadGuard<'_, Inner>> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(stopped());
        }
        Ok(self.inner.read().unwrap_or_else(PoisonError::into_inner))
    }

    fn write_lock(&self) -> Result<RwLockWriteGuard<'_, Inner>> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(stopped());
        }
        let inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        inner.writable()?;
        Ok(inner)
    }

    /// Runs steps 3 to 7 of the checkpoint of spec/08 section 8.6.2. The caller flushes the buffer pool first, so that each dirty page is in the store. An error stops the store.
    pub fn checkpoint(&self, c: &Checkpoint) -> Result<()> {
        let mut inner = self.write_lock()?;
        let result = inner.checkpoint(&*self.file, c);
        if result.is_err() {
            self.stopped.store(true, Ordering::Release);
        }
        result
    }

    /// The active header slot.
    pub fn slot(&self) -> (SlotName, Slot) {
        let inner = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        (inner.active, inner.slot.clone())
    }

    /// The counts.
    pub fn stats(&self) -> FileStats {
        let inner = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        FileStats {
            file_pages: inner.file_pages(),
            next_page: inner.next_page,
            used_pages: inner
                .arenas
                .iter()
                .map(|a| ARENA_PAGES - u64::from(a.map.free_pages()))
                .sum(),
            pending_pages: (inner.pending_now.len() + inner.pending_prev.len()) as u64,
            reads: self.reads.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            generation: inner.slot.generation,
        }
    }

    /// Checks that the free space map in memory marks exactly the physical pages in use. `rupg check` and the tests call it. A mismatch gives SQLSTATE `XX000`.
    pub fn check(&self) -> Result<()> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner).check()
    }
}

impl Store for FileStore {
    fn read(&self, page: PageId, into: &mut Page) -> Result<()> {
        let inner = self.read_lock()?;
        let value = inner.entry(page.0);
        let Some(at) = value.physical() else {
            if inner.is_allocated(page.0) {
                // A page that was allocated and never written.
                into.fill(0);
                return Ok(());
            }
            return Err(not_allocated(page));
        };
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.file.read_at(offset(at), &mut into[..])?;
        verify(into, at, page, Some(value.tag())).map(|_| ())
    }

    fn write(&self, page: PageId, from: &Page) -> Result<()> {
        let mut inner = self.write_lock()?;
        inner.write(&*self.file, page, from)?;
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn allocate(&self) -> Result<PageId> {
        self.write_lock()?.allocate()
    }

    fn free(&self, page: PageId) -> Result<()> {
        self.write_lock()?.free_page(page)
    }
}

// The store has no unsafe code, and each test writes simulated files of 16 MiB or more, which is too slow under Miri.
#[cfg(all(test, not(miri)))]
mod tests {
    use std::path::Path;
    use std::sync::atomic::AtomicI64;

    use rupg_file::{PageAccess, pt_capacity};
    use rupg_platform::sim::{CrashPlan, Fate, Faults, SimIo};
    use rupg_platform::{Io, MemoryPool, OpenMode};

    use super::*;
    use crate::{BufferPool, PoolConfig, PoolMode};

    const ID: FileId = [5; 16];
    const PATH: &str = "db";

    fn ts(n: u64) -> Hlc {
        Hlc::new(1000 + n, 0).unwrap()
    }

    /// A page with a header and the logical page number and a version in the body. The timestamp changes with the version, so each copy has its own write tag.
    fn page_of(n: u64, version: u64) -> Box<Page> {
        let mut page = Box::new([0u8; PAGE_SIZE]);
        let mut header = PageHeader::new(PageId(n), PageKind::HotLeaf);
        header.timestamp = Hlc::from_bits(version * 7919 + n);
        header.write(&mut page);
        page[100..108].copy_from_slice(&n.to_le_bytes());
        page[108..116].copy_from_slice(&version.to_le_bytes());
        page
    }

    fn version(store: &FileStore, n: u64) -> Result<u64> {
        let mut page = Box::new([0u8; PAGE_SIZE]);
        store.read(PageId(n), &mut page)?;
        assert_eq!(page[100..108], n.to_le_bytes());
        Ok(u64::from_le_bytes(page[108..116].try_into().unwrap()))
    }

    /// The version of each allocated page.
    type Model = BTreeMap<u64, u64>;

    fn write(store: &FileStore, model: &mut Model, n: u64, v: u64) {
        store.write(PageId(n), &page_of(n, v)).unwrap();
        model.insert(n, v);
    }

    fn matches(store: &FileStore, model: &Model) {
        for (&n, &v) in model {
            assert_eq!(version(store, n).unwrap(), v, "logical page {n}");
        }
        store.check().unwrap();
    }

    fn create(io: &SimIo) -> FileStore {
        FileStore::create(io.open(Path::new(PATH), OpenMode::CreateNew).unwrap(), ID, ts(0))
            .unwrap()
    }

    fn open(io: &SimIo) -> Result<FileStore> {
        FileStore::open(io.open(Path::new(PATH), OpenMode::ReadWrite)?)
    }

    fn checkpoint(store: &FileStore, n: u64) {
        store.checkpoint(&Checkpoint { timestamp: ts(n), ..Checkpoint::default() }).unwrap();
    }

    #[test]
    fn create_and_open() {
        let io = SimIo::new(1);
        let s = create(&io);
        assert_eq!((s.slot().0, s.slot().1.generation), (SlotName::A, 1));
        assert_eq!(s.stats().file_pages, FIRST_ARENA_PAGE + ARENA_PAGES);
        s.check().unwrap();
        let pages: Vec<u64> = (0..3).map(|_| s.allocate().unwrap().0).collect();
        assert_eq!(pages, [1, 2, 3]);
        let mut model = Model::new();
        write(&s, &mut model, 1, 1);
        write(&s, &mut model, 2, 1);
        matches(&s, &model);
        // A page that was allocated and never written reads as zero bytes.
        let mut page = Box::new([1u8; PAGE_SIZE]);
        s.read(PageId(3), &mut page).unwrap();
        assert!(page.iter().all(|&b| b == 0));
        assert_eq!(s.read(PageId(9), &mut page).unwrap_err().state(), SqlState::INTERNAL_ERROR);
        checkpoint(&s, 1);
        assert_eq!((s.slot().0, s.slot().1.generation), (SlotName::B, 2));
        matches(&s, &model);
        drop(s);

        let s = open(&io).unwrap();
        assert_eq!((s.slot().0, s.slot().1.generation), (SlotName::B, 2));
        matches(&s, &model);
        // Page 3 has no copy, so it is free after the open.
        assert_eq!(s.allocate().unwrap(), PageId(3));
        assert_eq!(s.allocate().unwrap(), PageId(4));
    }

    #[test]
    fn bad_calls() {
        let io = SimIo::new(2);
        let s = create(&io);
        let p = s.allocate().unwrap();
        let e = s.write(p, &page_of(p.0 + 1, 1)).unwrap_err();
        assert_eq!(e.state(), SqlState::INTERNAL_ERROR);
        assert!(e.message().contains("names logical page 2"), "{e}");
        let e = s.write(p, &[0u8; PAGE_SIZE]).unwrap_err();
        assert_eq!(e.state(), SqlState::INTERNAL_ERROR);
        assert_eq!(
            s.write(PageId(7), &page_of(7, 1)).unwrap_err().state(),
            SqlState::INTERNAL_ERROR
        );
        s.free(p).unwrap();
        assert_eq!(s.free(p).unwrap_err().state(), SqlState::INTERNAL_ERROR);
        // A released number is given out again only after the next checkpoint.
        assert_eq!(s.allocate().unwrap(), PageId(2));
        checkpoint(&s, 1);
        assert_eq!(s.allocate().unwrap(), PageId(1));
        s.check().unwrap();
        let file = io.open(Path::new("other"), OpenMode::CreateNew).unwrap();
        file.write_at(0, b"x").unwrap();
        let e = FileStore::create(file, ID, ts(0)).unwrap_err();
        assert_eq!(e.state(), SqlState::DUPLICATE_FILE);
    }

    #[test]
    fn space_is_used_again() {
        let io = SimIo::new(3);
        let s = create(&io);
        let mut model = Model::new();
        for _ in 0..5 {
            s.allocate().unwrap();
        }
        for round in 1..=60 {
            for n in 1..=5 {
                write(&s, &mut model, n, round);
            }
            if round % 3 == 0 {
                checkpoint(&s, round);
                matches(&s, &model);
            }
        }
        let stats = s.stats();
        // 5 pages and their metadata never need a second arena.
        assert_eq!(stats.file_pages, FIRST_ARENA_PAGE + ARENA_PAGES);
        assert!(stats.used_pages < 64, "{stats:?}");
        assert_eq!(stats.writes, 300);
        drop(s);
        let s = open(&io).unwrap();
        matches(&s, &model);
    }

    #[test]
    fn the_file_grows_by_arenas() {
        let io = SimIo::new(4);
        let s = create(&io);
        let mut model = Model::new();
        for v in 0..1100 {
            let n = s.allocate().unwrap().0;
            write(&s, &mut model, n, v);
        }
        checkpoint(&s, 1);
        assert_eq!(s.stats().file_pages, FIRST_ARENA_PAGE + 2 * ARENA_PAGES);
        matches(&s, &model);
        drop(s);
        let s = open(&io).unwrap();
        matches(&s, &model);
        assert_eq!(s.stats().next_page, 1101);
    }

    #[test]
    fn the_old_slot_stays_valid() {
        let io = SimIo::new(5);
        let s = create(&io);
        let mut x = Model::new();
        for n in 1..=4 {
            s.allocate().unwrap();
            write(&s, &mut x, n, 1);
        }
        checkpoint(&s, 1);
        let mut y = x.clone();
        for n in 1..=4 {
            write(&s, &mut y, n, 2);
        }
        checkpoint(&s, 2);
        let (name, slot) = s.slot();
        assert_eq!(slot.generation, 3);
        // Many writes after the checkpoint use each free page. None of them may be a page that the older slot uses.
        for v in 3..700 {
            for n in 1..=4 {
                write(&s, &mut y, n, v);
            }
        }
        drop(s);
        let file = io.open(Path::new(PATH), OpenMode::ReadWrite).unwrap();
        file.write_at(offset(name.page()) + 100, &[0xff; 8]).unwrap();
        file.sync().unwrap();
        let s = open(&io).unwrap();
        assert_eq!((s.slot().0, s.slot().1.generation), (name.other(), 2));
        matches(&s, &x);
    }

    #[test]
    fn damaged_pages() {
        let io = SimIo::new(6);
        let s = create(&io);
        let mut model = Model::new();
        s.allocate().unwrap();
        write(&s, &mut model, 1, 1);
        checkpoint(&s, 1);
        let at = s.inner.read().unwrap().entry(1).physical().unwrap();
        let root = s.slot().1.page_table.page;
        drop(s);
        let file = io.open(Path::new(PATH), OpenMode::ReadWrite).unwrap();
        file.write_at(offset(at) + 200, &[1]).unwrap();
        let s = open(&io).unwrap();
        let mut page = Box::new([0u8; PAGE_SIZE]);
        assert_eq!(s.read(PageId(1), &mut page).unwrap_err().state(), SqlState::DATA_CORRUPTED);
        drop(s);
        file.write_at(offset(root) + 200, &[1]).unwrap();
        assert_eq!(open(&io).unwrap_err().state(), SqlState::DATA_CORRUPTED);
    }

    #[test]
    fn a_failed_sync_stops_the_store() {
        let io = SimIo::new(7);
        let s = create(&io);
        let mut model = Model::new();
        s.allocate().unwrap();
        write(&s, &mut model, 1, 1);
        io.set_faults(Faults { sync_error: 1_000_000, ..Faults::default() });
        assert_eq!(
            s.checkpoint(&Checkpoint { timestamp: ts(1), ..Checkpoint::default() })
                .unwrap_err()
                .state(),
            SqlState::IO_ERROR
        );
        let e = version(&s, 1).unwrap_err();
        assert!(e.message().contains("stopped"), "{e}");
        assert_eq!(s.allocate().unwrap_err().state(), SqlState::IO_ERROR);
    }

    #[test]
    fn the_table_grows_a_level() {
        let io = SimIo::new(8);
        let s = create(&io);
        let cap = pt_capacity(PT_MIN_LEVELS);
        s.inner.write().unwrap().next_page = cap - 1;
        let mut model = Model::new();
        for n in [cap - 1, cap] {
            assert_eq!(s.allocate().unwrap(), PageId(n));
            write(&s, &mut model, n, 1);
        }
        assert_eq!(s.inner.read().unwrap().levels, PT_MIN_LEVELS + 1);
        checkpoint(&s, 1);
        matches(&s, &model);
        let root = s.slot().1.page_table.page;
        let mut page = Box::new([0u8; PAGE_SIZE]);
        io.open(Path::new(PATH), OpenMode::Read)
            .unwrap()
            .read_at(offset(root), &mut page[..])
            .unwrap();
        assert_eq!(PtNode::level(&page), PT_MIN_LEVELS);
    }

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

    /// Changes that make state `y` from state `x`. An error is the death of the process.
    fn change(s: &FileStore, y: &mut Model) -> Result<()> {
        for n in 1..=3 {
            s.write(PageId(n), &page_of(n, 2))?;
            y.insert(n, 2);
        }
        for _ in 0..2 {
            let n = s.allocate()?.0;
            s.write(PageId(n), &page_of(n, 2))?;
            y.insert(n, 2);
        }
        s.free(PageId(4))?;
        y.remove(&4);
        s.checkpoint(&Checkpoint { timestamp: ts(2), ..Checkpoint::default() })
    }

    #[test]
    fn crash_at_each_step() {
        let mut cases = 0;
        for seed in 0..4 {
            for cut in 0i64.. {
                let io = SimIo::new(seed * 1000 + cut as u64);
                let file = Arc::new(Cut {
                    inner: io.open(Path::new(PATH), OpenMode::CreateNew).unwrap(),
                    left: AtomicI64::new(i64::MAX),
                });
                let s = FileStore::create(Arc::clone(&file) as Arc<dyn File>, ID, ts(0)).unwrap();
                let mut x = Model::new();
                for n in 1..=6 {
                    s.allocate().unwrap();
                    write(&s, &mut x, n, 1);
                }
                s.free(PageId(6)).unwrap();
                x.remove(&6);
                checkpoint(&s, 1);

                file.left.store(cut, Ordering::SeqCst);
                let mut y = x.clone();
                let done = change(&s, &mut y).is_ok();
                drop(s);
                let plan = io.crash_random().unwrap();
                cases += 1;

                let s = open(&io).unwrap_or_else(|e| panic!("seed {seed} cut {cut} {plan:?}: {e}"));
                let generation = s.slot().1.generation;
                if done {
                    assert_eq!(generation, 3);
                }
                matches(&s, if generation == 3 { &y } else { &x });
                if generation == 3 {
                    assert_eq!(version(&s, 4).unwrap_err().state(), SqlState::INTERNAL_ERROR);
                }
                // The store works after the crash.
                let mut z = if generation == 3 { y } else { x };
                write(&s, &mut z, 1, 9);
                checkpoint(&s, 3);
                drop(s);
                let s = open(&io).unwrap();
                matches(&s, &z);
                if done {
                    break;
                }
            }
        }
        assert!(cases > 40, "{cases}");
    }

    #[test]
    fn crash_with_every_write_lost_or_kept() {
        for fate in [Fate::Lost, Fate::Written] {
            let io = SimIo::new(9);
            let s = create(&io);
            let mut x = Model::new();
            s.allocate().unwrap();
            write(&s, &mut x, 1, 1);
            checkpoint(&s, 1);
            let mut y = x.clone();
            write(&s, &mut y, 1, 2);
            drop(s);
            io.crash(&CrashPlan { fates: vec![fate; io.unsynced().len()], order: Vec::new() })
                .unwrap();
            // The slot names the copy of the checkpoint in both cases.
            matches(&open(&io).unwrap(), &x);
        }
    }

    #[test]
    fn with_the_pool() {
        let io = SimIo::new(10);
        let store = Arc::new(create(&io));
        let memory = MemoryPool::new(64 << 20);
        let config = PoolConfig {
            shared_buffers: 32 * PAGE_SIZE as u64,
            mode: Some(PoolMode::Table),
            window_pages: None,
        };
        let pool = BufferPool::new(Arc::clone(&store) as Arc<dyn Store>, &memory, config).unwrap();
        let mut pages = Vec::new();
        for v in 0..100u64 {
            let (p, mut guard) = pool.allocate().unwrap();
            guard.copy_from_slice(&page_of(p.0, v)[..]);
            pages.push((p, v));
        }
        pool.flush().unwrap();
        store.checkpoint(&Checkpoint { timestamp: ts(1), ..Checkpoint::default() }).unwrap();
        drop(pool);
        drop(store);

        let store = Arc::new(open(&io).unwrap());
        let pool = BufferPool::new(Arc::clone(&store) as Arc<dyn Store>, &memory, config).unwrap();
        for (p, v) in pages {
            let guard = pool.shared(p).unwrap();
            assert_eq!(guard[108..116], v.to_le_bytes());
        }
        store.check().unwrap();
    }
}
