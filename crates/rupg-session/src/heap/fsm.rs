//! The free space map of a heap, as `freespace.c` and `fsmpage.c` keep it: a tree of pages of three levels, each page a binary tree whose nodes have the most free space of their children, with the free space of each page of the heap in the leaves of the pages of the bottom level. The free space is a category of 32 bytes.

use std::collections::BTreeMap;

/// `NodesPerPage`: the nodes of the binary tree of a page of 8 kB.
const NODES: usize = 8164;

/// `NonLeafNodesPerPage`.
const NON_LEAF: usize = 4095;

/// `SlotsPerFSMPage`: the leaves of a page.
const SLOTS: u32 = 4069;

/// `FSM_TREE_DEPTH`.
const DEPTH: u32 = 3;

/// `FSM_ROOT_LEVEL`.
const ROOT_LEVEL: u32 = DEPTH - 1;

/// `FSM_CAT_STEP`.
const CAT_STEP: usize = 32;

/// `MaxFSMRequestSize`: `MaxHeapTupleSize`.
const MAX_REQUEST: usize = 8160;

/// `FSMAddress`: a page of the map by its level and its place in the level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Address {
    level: u32,
    page: u32,
}

/// `FSM_ROOT_ADDRESS`.
const ROOT: Address = Address { level: ROOT_LEVEL, page: 0 };

/// `fsm_get_location`: the page of the bottom level and the slot of the heap page `block`.
fn location(block: u32) -> (Address, usize) {
    (Address { level: 0, page: block / SLOTS }, (block % SLOTS) as usize)
}

/// `fsm_get_child`.
fn child(parent: Address, slot: usize) -> Address {
    Address { level: parent.level - 1, page: parent.page * SLOTS + slot as u32 }
}

/// `fsm_get_parent`: the page above and the slot of `child` in it.
fn parent(child: Address) -> (Address, usize) {
    (Address { level: child.level + 1, page: child.page / SLOTS }, (child.page % SLOTS) as usize)
}

/// `fsm_get_heap_blk`.
fn heap_block(addr: Address, slot: usize) -> u32 {
    addr.page * SLOTS + slot as u32
}

/// `fsm_logical_to_physical`: the block of the page in the file of the map, each page after the pages above it.
fn physical(addr: Address) -> u32 {
    let mut leaf = addr.page;
    for _ in 0..addr.level {
        leaf *= SLOTS;
    }
    let mut pages = 0;
    for _ in 0..DEPTH {
        pages += leaf + 1;
        leaf /= SLOTS;
    }
    pages - addr.level - 1
}

/// `fsm_space_avail_to_cat`.
pub(super) fn avail_to_cat(avail: usize) -> u8 {
    if avail >= MAX_REQUEST { 255 } else { (avail / CAT_STEP).min(254) as u8 }
}

/// `fsm_space_needed_to_cat`.
pub(super) fn needed_to_cat(needed: usize) -> u8 {
    if needed == 0 { 1 } else { needed.div_ceil(CAT_STEP).min(255) as u8 }
}

/// `parentof`.
fn parent_of(x: usize) -> usize {
    (x - 1) / 2
}

/// `leftchild`.
fn left_child(x: usize) -> usize {
    2 * x + 1
}

/// `rightneighbor`: the node at the right of `x`, which wraps around to the first node of the level below.
fn right_neighbor(x: usize) -> usize {
    let x = x + 1;
    if (x + 1) & x == 0 { parent_of(x) } else { x }
}

/// `FSMPageData`.
#[derive(Clone, Debug)]
struct FsmPage {
    nodes: Box<[u8]>,
    /// `fp_next_slot`: the slot where the next search starts.
    next_slot: usize,
}

impl Default for FsmPage {
    fn default() -> Self {
        FsmPage { nodes: vec![0; NODES].into_boxed_slice(), next_slot: 0 }
    }
}

impl FsmPage {
    /// `fsm_set_avail`: sets the leaf `slot` to `value`, and the nodes above it.
    fn set_avail(&mut self, slot: usize, value: u8) {
        let mut node = NON_LEAF + slot;
        if self.nodes[node] == value && value <= self.nodes[0] {
            return;
        }
        self.nodes[node] = value;
        while node > 0 {
            node = parent_of(node);
            let left = left_child(node);
            let mut new = self.nodes[left];
            if left + 1 < NODES {
                new = new.max(self.nodes[left + 1]);
            }
            if self.nodes[node] == new {
                break;
            }
            self.nodes[node] = new;
        }
        if value > self.nodes[0] {
            self.rebuild();
        }
    }

    /// `fsm_search_avail`: a slot with at least `min`, from `fp_next_slot` to the right, which wraps around. The search moves `fp_next_slot` after the slot on the bottom level, when `advance` is true, and to the slot on the other levels.
    fn search_avail(&mut self, min: u8, advance: bool) -> Option<usize> {
        if self.nodes[0] < min {
            return None;
        }
        let target = if self.next_slot < SLOTS as usize { self.next_slot } else { 0 };
        let mut node = target + NON_LEAF;
        while node > 0 && self.nodes[node] < min {
            node = parent_of(right_neighbor(node));
        }
        while node < NON_LEAF {
            let left = left_child(node);
            if left < NODES && self.nodes[left] >= min {
                node = left;
            } else if left + 1 < NODES && self.nodes[left + 1] >= min {
                node = left + 1;
            } else {
                return None;
            }
        }
        let slot = node - NON_LEAF;
        self.next_slot = slot + usize::from(advance);
        Some(slot)
    }

    /// `fsm_rebuild_page`.
    fn rebuild(&mut self) {
        for node in (0..NON_LEAF).rev() {
            let left = left_child(node);
            let mut new = if left < NODES { self.nodes[left] } else { 0 };
            if left + 1 < NODES {
                new = new.max(self.nodes[left + 1]);
            }
            self.nodes[node] = new;
        }
    }

    /// `fsm_truncate_avail`: clears the leaves from `slots` on.
    fn truncate_avail(&mut self, slots: usize) {
        let leaves = &mut self.nodes[NON_LEAF + slots..];
        if leaves.iter().any(|&n| n != 0) {
            leaves.fill(0);
            self.rebuild();
        }
    }
}

/// The free space map fork of a heap. A page that no write touched is a new page, which has no free space.
#[derive(Clone, Debug, Default)]
pub(super) struct Fsm {
    /// The length of the fork in pages.
    blocks: u32,
    pages: BTreeMap<u32, FsmPage>,
}

impl Fsm {
    /// The length of the fork in pages.
    pub(super) fn blocks(&self) -> u32 {
        self.blocks
    }

    /// `fsm_readbuf`: the page `addr`, or `None` past the end of the fork when `extend` is false. With `extend`, the fork grows to the page, as `fsm_extend` adds new pages.
    fn page(&mut self, addr: Address, extend: bool) -> Option<&mut FsmPage> {
        let block = physical(addr);
        if block >= self.blocks {
            if !extend {
                return None;
            }
            self.blocks = block + 1;
        }
        Some(self.pages.entry(block).or_default())
    }

    /// `fsm_set_and_search`: sets the slot `slot` of the page `addr` to `value`, then searches the page for a slot with at least `min` when `min` is not 0.
    fn set_and_search(&mut self, addr: Address, slot: usize, value: u8, min: u8) -> Option<usize> {
        let page = self.page(addr, true)?;
        page.set_avail(slot, value);
        if min == 0 { None } else { page.search_avail(min, addr.level == 0) }
    }

    /// `fsm_search`: a page of the heap of `blocks` pages with at least `min`, from the root down. A page of the level above that has a stale value gets the value of the page below, and the search starts again.
    fn search(&mut self, min: u8, blocks: u32) -> Option<u32> {
        let mut addr = ROOT;
        for _ in 0..10_000 {
            let (slot, max) = match self.page(addr, false) {
                Some(page) => match page.search_avail(min, addr.level == 0) {
                    Some(slot) => (Some(slot), 0),
                    None => (None, page.nodes[0]),
                },
                None => (None, 0),
            };
            match slot {
                Some(slot) if addr.level == 0 => {
                    let block = heap_block(addr, slot);
                    if block < blocks {
                        return Some(block);
                    }
                    if let Some(page) = self.page(addr, false) {
                        page.set_avail(slot, 0);
                    }
                    addr = ROOT;
                }
                Some(slot) => addr = child(addr, slot),
                None if addr.level == ROOT_LEVEL => return None,
                None => {
                    let (up, slot) = parent(addr);
                    self.set_and_search(up, slot, max, 0);
                    addr = ROOT;
                }
            }
        }
        None
    }

    /// `GetPageWithFreeSpace`: a page of the heap of `blocks` pages with `needed` bytes of free space.
    pub(super) fn get(&mut self, needed: usize, blocks: u32) -> Option<u32> {
        self.search(needed_to_cat(needed), blocks)
    }

    /// `RecordAndGetPageWithFreeSpace`: records the free space `avail` of the page `old`, then gives a page with `needed` bytes, first from the same page of the map.
    pub(super) fn record_and_get(
        &mut self,
        old: u32,
        avail: usize,
        needed: usize,
        blocks: u32,
    ) -> Option<u32> {
        let (addr, slot) = location(old);
        let min = needed_to_cat(needed);
        if let Some(found) = self.set_and_search(addr, slot, avail_to_cat(avail), min) {
            let block = heap_block(addr, found);
            if block < blocks {
                return Some(block);
            }
        }
        self.search(min, blocks)
    }

    /// `RecordPageWithFreeSpace`.
    pub(super) fn record(&mut self, block: u32, avail: usize) {
        let (addr, slot) = location(block);
        self.set_and_search(addr, slot, avail_to_cat(avail), 0);
    }

    /// `FreeSpaceMapVacuumRange`: the pages above the heap pages from `start` to before `end` get the values of the pages below them.
    pub(super) fn vacuum_range(&mut self, start: u32, end: u32) {
        if end > start {
            let mut eof = false;
            self.vacuum_page(ROOT, start, end, &mut eof);
        }
    }

    /// `fsm_vacuum_page`: the most free space of the page `addr`, after its slots from `start` to before `end` get the values of their children. The search of each page that it reads starts again from the first slot.
    fn vacuum_page(&mut self, addr: Address, start: u32, end: u32, eof: &mut bool) -> u8 {
        if self.page(addr, false).is_none() {
            *eof = true;
            return 0;
        }
        *eof = false;
        if addr.level > 0 {
            let (mut first, mut first_slot) = location(start);
            let (mut last, mut last_slot) = location(end - 1);
            while first.level < addr.level {
                (first, first_slot) = parent(first);
                (last, last_slot) = parent(last);
            }
            let start_slot = match first.page.cmp(&addr.page) {
                std::cmp::Ordering::Equal => first_slot,
                std::cmp::Ordering::Greater => SLOTS as usize,
                std::cmp::Ordering::Less => 0,
            };
            let end_slot = match last.page.cmp(&addr.page) {
                std::cmp::Ordering::Equal => Some(last_slot),
                std::cmp::Ordering::Greater => Some(SLOTS as usize - 1),
                std::cmp::Ordering::Less => None,
            };
            let mut done = false;
            for slot in start_slot..end_slot.map_or(start_slot, |e| e + 1) {
                let avail = if done {
                    0
                } else {
                    self.vacuum_page(child(addr, slot), start, end, &mut done)
                };
                if let Some(page) = self.page(addr, false)
                    && page.nodes[NON_LEAF + slot] != avail
                {
                    page.set_avail(slot, avail);
                }
            }
        }
        let page = self.page(addr, false).expect("the page was read");
        page.next_slot = 0;
        page.nodes[0]
    }

    /// `FreeSpaceMapPrepareTruncateRel` and `FreeSpaceMapVacuumRange` of `RelationTruncate`: the map after the heap has `blocks` pages. The map keeps the page of the new last heap page, with no free space for the pages after it.
    pub(super) fn truncate(&mut self, blocks: u32) {
        if self.blocks == 0 {
            return;
        }
        let (addr, slot) = location(blocks);
        let length = if slot > 0 {
            let Some(page) = self.page(addr, false) else { return };
            page.truncate_avail(slot);
            physical(addr) + 1
        } else {
            let length = physical(addr);
            if self.blocks <= length {
                return;
            }
            length
        };
        self.blocks = length;
        self.pages.retain(|&block, _| block < length);
        self.vacuum_range(blocks, u32::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pages_of_the_map_follow_the_pages_above_them() {
        assert_eq!(physical(ROOT), 0);
        assert_eq!(physical(Address { level: 1, page: 0 }), 1);
        assert_eq!(physical(Address { level: 0, page: 0 }), 2);
        assert_eq!(physical(Address { level: 0, page: 1 }), 3);
    }

    #[test]
    fn a_search_finds_the_next_page_with_space() {
        let mut fsm = Fsm::default();
        for block in 0..10 {
            fsm.record(block, if block % 3 == 0 { 4000 } else { 10 });
        }
        assert_eq!(fsm.blocks(), 3);
        assert_eq!(fsm.get(100, 10), None);
        fsm.vacuum_range(0, 10);
        assert_eq!(fsm.get(100, 10), Some(0));
        assert_eq!(fsm.record_and_get(0, 0, 100, 10), Some(3));
        assert_eq!(fsm.record_and_get(3, 0, 100, 10), Some(6));
        fsm.truncate(4);
        assert_eq!(fsm.get(100, 4), None);
    }
}
