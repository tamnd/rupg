//! The pages of the heap of a table: the storage of a version in a page, the free space of each page as `RelationGetBufferForTuple` fills them, the free space map and the visibility map, and `relpages` and `reltuples` of the table.

use rupg_types::Value;

use super::fsm::Fsm;

/// `MaxHeapTuplesPerPage`: the most versions that a page of 8 kB has. The ID of a version gives its page and its place in the page as if each page had this number of versions.
pub(super) const TUPLES_PER_PAGE: u64 = 291;

/// `BLCKSZ`.
const PAGE_SIZE: u16 = 8192;

/// `SizeOfPageHeaderData`.
const PAGE_HEADER: u16 = 24;

/// `sizeof(ItemIdData)`: the line pointer of a version in its page.
const LINE_POINTER: u16 = 4;

/// `SizeofHeapTupleHeader`.
const TUPLE_HEADER: usize = 23;

/// `MAX_BUFFERS_TO_EXTEND_BY`: the most pages that one extension of a bulk insert adds.
const MAX_EXTEND: u32 = 64;

/// `TOAST_TUPLE_THRESHOLD` and `TOAST_TUPLE_TARGET`: a version longer than this has its longest values in the TOAST table.
const TOAST_THRESHOLD: usize = 2032;

/// `TOAST_POINTER_SIZE`: the length of a value in the TOAST table in its version, with its header of one byte.
const TOAST_POINTER: usize = 18;

/// `MAXALIGN`.
pub(super) fn max_align(len: usize) -> usize {
    (len + 7) & !7
}

/// `att_align_nominal`: `len` up to a multiple of `align`.
fn align_to(len: usize, align: usize) -> usize {
    len.div_ceil(align) * align
}

/// The storage of a value in a version: the length and the alignment of a value of fixed length, or the length of the data of a `varlena` value with the alignment of its type.
#[derive(Clone, Copy)]
enum Layout {
    Fixed(usize, usize),
    Varlena(usize, usize),
}

/// The storage of a value as the type of its variant stores it. `Text` has the storage of `text`, so a value of `name` takes less space than in PostgreSQL. The types whose storage the engine does not follow take 16 bytes of data.
fn layout(value: &Value) -> Layout {
    match value {
        Value::Null => Layout::Fixed(0, 1),
        Value::Bool(_) | Value::Char(_) => Layout::Fixed(1, 1),
        Value::Int2(_) => Layout::Fixed(2, 2),
        Value::Int4(_) | Value::Float4(_) | Value::Oid(_) | Value::Date(_) => Layout::Fixed(4, 4),
        Value::Int8(_)
        | Value::Float8(_)
        | Value::Time(_)
        | Value::Timestamp(_)
        | Value::TimestampTz(_) => Layout::Fixed(8, 8),
        Value::TimeTz(..) => Layout::Fixed(12, 8),
        Value::Interval(_) => Layout::Fixed(16, 8),
        Value::Uuid(_) => Layout::Fixed(16, 1),
        Value::Text(text) => Layout::Varlena(text.len(), 4),
        Value::Bytea(bytes) => Layout::Varlena(bytes.len(), 4),
        // `make_result`: a header of two bytes when `NUMERIC_CAN_BE_SHORT`, and four bytes otherwise, then the digits.
        Value::Numeric(n) => {
            let short = n.dscale() <= 63 && (-64..=63).contains(&n.weight());
            let header = if short || n.digits().is_empty() { 2 } else { 4 };
            Layout::Varlena(header + 2 * n.digits().len(), 4)
        }
        Value::Inet(inet) => Layout::Varlena(2 + inet.family.size(), 4),
        // `construct_md_array`: the header with the dimensions and the null bitmap, then each element at its alignment, a `varlena` element with a header of four bytes.
        Value::Array(array) => {
            let dims = 16 + 8 * array.dims.len();
            let nulls = array.values.iter().any(Option::is_none);
            let header =
                max_align(if nulls { dims + array.values.len().div_ceil(8) } else { dims });
            let (mut len, mut align) = (header, 4);
            for value in array.values.iter().flatten() {
                let (size, a) = match layout(value) {
                    Layout::Fixed(size, a) => (size, a),
                    Layout::Varlena(size, a) => (size + 4, a),
                };
                len = align_to(len, a) + size;
                align = align.max(a);
            }
            Layout::Varlena(len - 4, align)
        }
        // A composite value is a `HeapTupleHeader` with its fields.
        Value::Record(record) => Layout::Varlena(tuple_len(&record.values) - 4, 8),
        _ => Layout::Varlena(16, 4),
    }
}

/// `heap_compute_data_size`: the length of the data of a version with the values `layouts`. A `varlena` value of up to 126 bytes has a header of one byte and no alignment, as `VARATT_CAN_MAKE_SHORT` lets it.
fn data_len(layouts: &[Layout]) -> usize {
    layouts.iter().fold(0, |len, layout| match *layout {
        Layout::Fixed(size, align) => align_to(len, align) + size,
        Layout::Varlena(size, _) if size < 127 => len + size + 1,
        Layout::Varlena(size, align) => align_to(len, align) + size + 4,
    })
}

/// `t_len` of a version with the values `values`, from `heap_form_tuple`: the header with the null bitmap, then the data. A version longer than `TOAST_TUPLE_THRESHOLD` has its longest values in the TOAST table, as `heap_toast_insert_or_update` moves them while the data is too long. A value that the TOAST table takes is not compressed first, as the engine has no `pglz`.
pub(super) fn tuple_len(values: &[Value]) -> usize {
    let nulls = values.iter().any(Value::is_null);
    let header = max_align(TUPLE_HEADER + if nulls { values.len().div_ceil(8) } else { 0 });
    let mut layouts: Vec<Layout> = values.iter().filter(|v| !v.is_null()).map(layout).collect();
    let mut len = data_len(&layouts);
    if header + len > TOAST_THRESHOLD {
        while header + len > TOAST_THRESHOLD {
            let longest = layouts
                .iter()
                .enumerate()
                .filter_map(|(at, layout)| match *layout {
                    Layout::Varlena(size, _) if size + 4 > max_align(TOAST_POINTER) => {
                        Some((size, at))
                    }
                    _ => None,
                })
                .max_by_key(|&(size, at)| (size, std::cmp::Reverse(at)));
            let Some((_, at)) = longest else { break };
            layouts[at] = Layout::Fixed(TOAST_POINTER, 1);
            len = data_len(&layouts);
        }
    }
    header + len
}

/// The lengths of the values `values` of a version, as `ANALYZE` reads them: `VARSIZE_ANY` of the value in the version, which is the length of the TOAST pointer for a value that [`tuple_len`] moves to the TOAST table, and `toast_raw_datum_size`, the length of the value with a header of four bytes. A null value and a value of fixed length give 0 for both.
pub(crate) fn stored_widths(values: &[Value]) -> Vec<(usize, usize)> {
    let nulls = values.iter().any(Value::is_null);
    let header = max_align(TUPLE_HEADER + if nulls { values.len().div_ceil(8) } else { 0 });
    let mut layouts: Vec<Layout> = values.iter().map(layout).collect();
    let mut toasted = vec![false; values.len()];
    let present = |layouts: &[Layout]| {
        let kept: Vec<Layout> = layouts
            .iter()
            .zip(values)
            .filter(|(_, v)| !v.is_null())
            .map(|(layout, _)| *layout)
            .collect();
        data_len(&kept)
    };
    while header + present(&layouts) > TOAST_THRESHOLD {
        let longest = layouts
            .iter()
            .enumerate()
            .filter(|&(at, _)| !values[at].is_null())
            .filter_map(|(at, layout)| match *layout {
                Layout::Varlena(size, _) if size + 4 > max_align(TOAST_POINTER) => Some((size, at)),
                _ => None,
            })
            .max_by_key(|&(size, at)| (size, std::cmp::Reverse(at)));
        let Some((_, at)) = longest else { break };
        layouts[at] = Layout::Fixed(TOAST_POINTER, 1);
        toasted[at] = true;
    }
    values
        .iter()
        .zip(layouts)
        .zip(toasted)
        .map(|((value, layout), toasted)| match layout {
            _ if value.is_null() => (0, 0),
            Layout::Fixed(..) if toasted => {
                let Layout::Varlena(size, _) = self::layout(value) else { return (0, 0) };
                (TOAST_POINTER, size + 4)
            }
            Layout::Fixed(..) => (0, 0),
            Layout::Varlena(size, _) if size < 127 => (size + 1, size + 4),
            Layout::Varlena(size, _) => (size + 4, size + 4),
        })
        .collect()
}

/// `HEAPBLOCKS_PER_PAGE` of `visibilitymap.c`: the heap pages that a page of the visibility map covers, with two bits for each.
const VM_HEAP_BLOCKS: u32 = (PAGE_SIZE as u32 - PAGE_HEADER as u32) * 4;

/// `SKIP_PAGES_THRESHOLD`: the fewest pages in a row that `VACUUM` skips.
const SKIP_PAGES: usize = 32;

/// `REL_TRUNCATE_MINIMUM`.
const TRUNCATE_MINIMUM: u32 = 1000;

/// `REL_TRUNCATE_FRACTION`.
const TRUNCATE_FRACTION: u32 = 16;

/// The words of the bit set of the line pointers of a page, one bit for each of `MaxHeapTuplesPerPage`.
const LINE_WORDS: usize = TUPLES_PER_PAGE.div_ceil(64) as usize;

/// The ID of the version in the slot `slot` of the page `page` of the heap of the generation `generation`. The item pointer of the version is the low 48 bits, `(page << 16) | slot`, and the generation tells the versions of a heap from the versions of the heap before a `TRUNCATE` in the same transaction.
pub(super) fn tid(generation: u16, page: u32, slot: u16) -> u64 {
    (u64::from(generation) << 48) | (u64::from(page) << 16) | u64::from(slot)
}

/// The page of the version `tid`.
pub(super) fn page_of(tid: u64) -> u32 {
    ((tid >> 16) & 0xffff_ffff) as u32
}

/// The slot of the version `tid` in its page.
pub(super) fn slot_of(tid: u64) -> u16 {
    (tid & 0xffff) as u16
}

/// The generation of the heap of the version `tid`.
pub(super) fn generation_of(tid: u64) -> u16 {
    (tid >> 48) as u16
}

/// A page of the heap of a table: `pd_lower` as the number of its line pointers, `pd_upper`, the line pointers that `VACUUM` made `LP_UNUSED`, and the bits of the page in the visibility map.
#[derive(Clone, Debug)]
pub(super) struct Page {
    lines: u16,
    upper: u16,
    /// The line pointers below `lines` that are `LP_UNUSED`, by their slot from 1.
    unused: Option<Box<[u64; LINE_WORDS]>>,
    /// `VISIBILITYMAP_ALL_VISIBLE`.
    visible: bool,
    /// `VISIBILITYMAP_ALL_FROZEN`.
    frozen: bool,
}

impl Page {
    /// `PageInit`.
    const NEW: Page =
        Page { lines: 0, upper: PAGE_SIZE, unused: None, visible: false, frozen: false };

    /// `pd_lower`.
    fn lower(&self) -> u16 {
        PAGE_HEADER + LINE_POINTER * self.lines
    }

    /// `PageGetHeapFreeSpace`: the space for one more version with its line pointer. A page with `MaxHeapTuplesPerPage` line pointers and none that is `LP_UNUSED` has none.
    pub(super) fn free(&self) -> usize {
        if u64::from(self.lines) >= TUPLES_PER_PAGE && self.unused.is_none() {
            return 0;
        }
        usize::from(self.upper - self.lower()).saturating_sub(LINE_POINTER.into())
    }

    /// `PageAddItemExtended`: the slot of a new version of `len` bytes, the first line pointer that is `LP_UNUSED` or a new one. A change of the page clears its bits in the visibility map, as `heap_insert` does.
    fn add(&mut self, len: usize) -> u16 {
        let slot = match self.unused.as_mut() {
            Some(bits) => {
                let word = bits.iter().position(|&w| w != 0).unwrap_or(0);
                let bit = bits[word].trailing_zeros();
                bits[word] &= !(1 << bit);
                if bits.iter().all(|&w| w == 0) {
                    self.unused = None;
                }
                (word * 64) as u16 + bit as u16 + 1
            }
            None => {
                self.lines += 1;
                self.lines
            }
        };
        self.upper -= u16::try_from(len).unwrap_or(self.upper).min(self.upper);
        self.visible = false;
        self.frozen = false;
        slot
    }
}

/// The extension of the heap by a bulk insert, `BulkInsertStateData`: the statement of the insert, the pages of the last extension that no version took yet, and `already_extended_by`.
#[derive(Clone, Copy, Debug)]
struct Bulk {
    made: (u32, u32),
    free: (u32, u32),
    extended_by: u32,
}

/// Which pages that the visibility map marks `VACUUM` skips.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Skip {
    /// `DISABLE_PAGE_SKIPPING`: none.
    None,
    /// An aggressive `VACUUM`, as `FREEZE` makes it: the pages that are all frozen.
    Frozen,
    /// The pages that are all visible.
    Visible,
}

/// The surviving versions of a page that `VACUUM` scanned: the slot and the length of each version, and the bits of the page in the visibility map.
pub(super) struct Pruned {
    pub(super) kept: Vec<(u16, u16)>,
    pub(super) visible: bool,
    pub(super) frozen: bool,
}

/// The pages of the heap of a table, as `RelationGetBufferForTuple` fills them, the free space map and the visibility map, and `relpages`, `reltuples`, `relallvisible` and `relallfrozen` of the table, which `vac_update_relstats` writes in place outside of the transactions.
#[derive(Clone, Debug)]
pub(super) struct Pages {
    pages: Vec<Page>,
    /// `RelationGetTargetBlock`: the page of the last new version.
    target: Option<u32>,
    bulk: Option<Bulk>,
    fsm: Fsm,
    /// The length of the visibility map fork in pages.
    vm_blocks: u32,
    pub(super) generation: u16,
    pub(super) relpages: i32,
    pub(super) reltuples: f32,
    pub(super) relallvisible: i32,
    pub(super) relallfrozen: i32,
}

impl Default for Pages {
    fn default() -> Self {
        Pages {
            pages: Vec::new(),
            target: None,
            bulk: None,
            fsm: Fsm::default(),
            vm_blocks: 0,
            generation: 0,
            relpages: 0,
            reltuples: -1.0,
            relallvisible: 0,
            relallfrozen: 0,
        }
    }
}

impl Pages {
    /// The pages of the new file of `RelationSetNewRelfilenumber`, with the next generation of IDs.
    pub(super) fn renewed(&self) -> Pages {
        Pages { generation: self.generation.wrapping_add(1), ..Pages::default() }
    }

    /// `RelationGetBufferForTuple`: the page and the slot of a new version of `len` bytes. An update puts the new version in the page `old` of the version that it replaces when it fits, as `heap_update` does. Then the version goes to the target page, or the page of the free space map, or the last page of the heap. A page with too little space goes to the free space map, which gives the next page to try, or the next free page of the last extension of a bulk insert. A bulk insert, the statement `bulk`, does not use the free space map, and adds as many pages as it added before, up to `MAX_BUFFERS_TO_EXTEND_BY`.
    pub(super) fn place(
        &mut self,
        len: usize,
        old: Option<u32>,
        bulk: Option<(u32, u32)>,
    ) -> (u32, u16) {
        if self.bulk.is_some_and(|b| Some(b.made) != bulk) {
            self.bulk = None;
        }
        let free = |pages: &[Page], at: u32| pages.get(at as usize).map_or(0, Page::free);
        if let Some(at) = old.filter(|&at| free(&self.pages, at) >= len) {
            return (at, self.pages[at as usize].add(len));
        }
        let use_fsm = bulk.is_none();
        let count = self.blocks();
        let mut at = self.target.filter(|&at| at < count);
        if at.is_none() && use_fsm {
            at = self.fsm.get(len, count);
        }
        if at.is_none() {
            at = count.checked_sub(1);
        }
        while let Some(block) = at {
            let space = free(&self.pages, block);
            if space >= len {
                self.target = Some(block);
                return (block, self.pages[block as usize].add(len));
            }
            at = if let Some(b) = self.bulk.as_mut().filter(|b| b.free.0 <= b.free.1) {
                b.free.0 += 1;
                Some(b.free.0 - 1)
            } else if !use_fsm {
                None
            } else {
                self.fsm.record_and_get(block, space, len, count)
            };
        }
        let by = match bulk {
            Some(made) => {
                let b = self.bulk.get_or_insert(Bulk { made, free: (1, 0), extended_by: 0 });
                let by = b.extended_by.clamp(1, MAX_EXTEND);
                b.extended_by += by;
                b.free = (count + 1, count + by - 1);
                by
            }
            None => 1,
        };
        self.pages.extend((0..by).map(|_| Page::NEW));
        self.target = Some(count);
        (count, self.pages[count as usize].add(len))
    }

    /// The page `at` changes, as a delete or an update of a version in it does, which clears its bits in the visibility map.
    pub(super) fn touch(&mut self, at: u32) {
        if let Some(page) = self.pages.get_mut(at as usize) {
            page.visible = false;
            page.frozen = false;
        }
    }

    /// `RelationGetNumberOfBlocks`.
    pub(super) fn blocks(&self) -> u32 {
        u32::try_from(self.pages.len()).unwrap_or(u32::MAX)
    }

    /// The lengths of the free space map and the visibility map forks in pages.
    pub(super) fn fork_blocks(&self) -> (u32, u32) {
        (self.fsm.blocks(), self.vm_blocks)
    }

    /// `visibilitymap_count`: the pages that are all visible and the pages that are all frozen.
    fn visible_count(&self) -> (i32, i32) {
        let visible = self.pages.iter().filter(|p| p.visible).count();
        let frozen = self.pages.iter().filter(|p| p.frozen).count();
        (i32::try_from(visible).unwrap_or(i32::MAX), i32::try_from(frozen).unwrap_or(i32::MAX))
    }

    /// `vac_update_relstats` of `ANALYZE` and `index_update_stats`, with `relallvisible` and `relallfrozen` from the visibility map. The update of `pg_class` resets the target page, as the invalidation of the relation cache clears `rd_targblock`.
    pub(super) fn set_stats(&mut self, relpages: i32, reltuples: f32) {
        self.relpages = relpages;
        self.reltuples = reltuples;
        (self.relallvisible, self.relallfrozen) = self.visible_count();
        self.target = None;
    }

    /// The pages that `VACUUM` scans, as `find_next_unskippable_block` skips the runs of at least `SKIP_PAGES_THRESHOLD` pages that the visibility map marks, and never the last page.
    pub(super) fn scanned(&self, skip: Skip) -> Vec<bool> {
        let skippable = |p: &Page| match skip {
            Skip::None => false,
            Skip::Frozen => p.frozen,
            Skip::Visible => p.visible,
        };
        let count = self.pages.len();
        let mut scanned = vec![true; count];
        let mut at = 0;
        while at < count {
            let start = at;
            while at + 1 < count && skippable(&self.pages[at]) {
                at += 1;
            }
            if at - start >= SKIP_PAGES {
                scanned[start..at].fill(false);
            }
            at += 1;
        }
        scanned
    }

    /// The page `at` after `VACUUM` removed its dead versions and the versions of the transactions that aborted: `PageRepairFragmentation` gives their space back and truncates the line pointers after the last that is used. With an index, the dead versions are `LP_DEAD` until `lazy_vacuum_heap_page` makes them `LP_UNUSED`, and `PageTruncateLinePointerArray` keeps the first line pointer.
    pub(super) fn prune(&mut self, at: u32, pruned: &Pruned, indexed: bool) {
        let Some(page) = self.pages.get_mut(at as usize) else { return };
        page.visible = pruned.visible;
        page.frozen = pruned.frozen;
        let used = pruned.kept.iter().map(|&(slot, _)| slot).max().unwrap_or(0);
        page.lines = if indexed { used.max(1).min(page.lines) } else { used };
        page.upper = PAGE_SIZE - pruned.kept.iter().map(|&(_, len)| len).sum::<u16>();
        let mut bits = [0_u64; LINE_WORDS];
        for slot in 1..=page.lines {
            let i = usize::from(slot - 1);
            bits[i / 64] |= 1 << (i % 64);
        }
        for &(slot, _) in &pruned.kept {
            let i = usize::from(slot - 1);
            bits[i / 64] &= !(1 << (i % 64));
        }
        page.unused = bits.iter().any(|&w| w != 0).then(|| Box::new(bits));
    }

    /// The end of `lazy_scan_heap` and `lazy_truncate_heap`: the free space of each page that `VACUUM` scanned goes to the free space map, and the visibility map covers the heap. The heap loses its empty pages at the end, from `nonempty` on, when there are at least `REL_TRUNCATE_MINIMUM` of them or a 16th of the heap, as `should_attempt_truncation` decides, and `truncate` lets it. The result is the pages of the heap before the truncation.
    pub(super) fn vacuumed(&mut self, scanned: &[bool], nonempty: u32, truncate: bool) -> u32 {
        let count = self.blocks();
        for (at, _) in scanned.iter().enumerate().filter(|(_, s)| **s) {
            let at = at as u32;
            self.fsm.record(at, self.pages[at as usize].free());
        }
        self.fsm.vacuum_range(0, count);
        if count > 0 {
            self.vm_blocks = self.vm_blocks.max(count.div_ceil(VM_HEAP_BLOCKS));
        }
        let freeable = count - nonempty.min(count);
        if truncate
            && freeable > 0
            && (freeable >= TRUNCATE_MINIMUM || freeable >= count / TRUNCATE_FRACTION)
        {
            self.pages.truncate(nonempty as usize);
            self.fsm.truncate(nonempty);
            self.vm_blocks = self.vm_blocks.min(nonempty.div_ceil(VM_HEAP_BLOCKS));
            if self.bulk.is_some_and(|b| b.free.1 >= nonempty) {
                self.bulk = None;
            }
        }
        self.target = None;
        count
    }

    /// `vac_update_relstats` at the end of `VACUUM`: `relpages`, `reltuples` from `vac_estimate_reltuples` of the `live` versions of the `scanned` pages of the heap of `total` pages, `relallvisible` and `relallfrozen`. The result is `reltuples`.
    pub(super) fn vacuum_stats(&mut self, total: u32, scanned: u32, live: f64) -> f32 {
        let (old_pages, old_tuples) = (self.relpages, f64::from(self.reltuples));
        let (total_f, scanned_f) = (f64::from(total), f64::from(scanned));
        let tuples = if scanned >= total {
            live
        } else if (old_pages >= 0
            && old_pages.cast_unsigned() == total
            && scanned_f < total_f * 0.02)
            || scanned <= 1
        {
            old_tuples
        } else if old_tuples < 0.0 || old_pages == 0 {
            (live / scanned_f * total_f + 0.5).floor()
        } else {
            let density = old_tuples / f64::from(old_pages);
            (density * (total_f - scanned_f) + live + 0.5).floor()
        };
        self.relpages = i32::try_from(self.blocks()).unwrap_or(i32::MAX);
        self.reltuples = tuples as f32;
        let (visible, frozen) = self.visible_count();
        self.relallvisible = visible.min(self.relpages);
        self.relallfrozen = frozen.min(self.relpages);
        self.reltuples
    }
}
