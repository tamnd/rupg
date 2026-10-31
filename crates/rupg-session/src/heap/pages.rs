//! The pages of the heap of a table: the storage of a version in a page, the free space of each page as `RelationGetBufferForTuple` fills them, the free space map and the visibility map, and `relpages` and `reltuples` of the table.

use rupg_common::persist::{Persist, Reader, corrupt};
use rupg_encode::Method;
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

/// `MaxHeapTupleSize`: the space of an empty page for one version.
const MAX_TUPLE: usize = PAGE_SIZE as usize - 32;

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

/// The storage of a value as the type of its variant stores it. `Text` has the storage of `text`, and a value of `name` gets the storage of its column from [`Fixed`]. The types whose storage the engine does not follow take 16 bytes of data.
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
        Value::Record(record) => Layout::Varlena(record_len(&record.values) - 4, 8),
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

/// `attlen` and `attalign` of a column with a type of fixed length, as bytes, or `None` for a column whose values have the storage of their variants.
pub(crate) type Fixed = Option<(usize, usize)>;

/// `attstorage` of a column, with the method that compresses its values, as `heap_toast_insert_or_update` reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Storage {
    /// `p`: the value stays in the version as it is, with a header of four bytes.
    Plain,
    /// `m`: the value is compressed, and goes to the TOAST table only when the version is longer than `TOAST_TUPLE_TARGET_MAIN`.
    Main(Method),
    /// `e`: the value goes to the TOAST table, and is not compressed.
    External,
    /// `x`: the value is compressed, and then goes to the TOAST table when the version is still too long.
    Extended(Method),
}

/// The storage of a column that the heap has no storage for: `text` with `lz4`, the default of `default_toast_compression`.
const DEFAULT_STORAGE: Storage = Storage::Extended(Method::Lz4);

/// `TOAST_TUPLE_TARGET_MAIN`, `MaxHeapTupleSize`: the length of a version up to which a value with the storage `m` stays in the version.
const TARGET_MAIN: usize = PAGE_SIZE as usize - 32;

/// The form of a `varlena` value in a version, `tai_colflags` and the value of `ToastAttrInfo`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    /// As it is, with a header of one byte when the column can take it.
    Raw,
    /// Compressed, with a header of eight bytes.
    Compressed,
    /// In the TOAST table, with a pointer of `TOAST_POINTER_SIZE` bytes in the version.
    External,
}

/// A value of a version in the TOAST model.
#[derive(Clone, Copy)]
enum Slot {
    Null,
    Fixed(usize, usize),
    Varlena {
        /// `tai_size`: `VARSIZE_ANY` of the value in its form.
        size: usize,
        /// The length of the value with a header of four bytes, `toast_raw_datum_size`.
        raw: usize,
        align: usize,
        held: Held,
        /// `TOASTCOL_INCOMPRESSIBLE`.
        incompressible: bool,
        /// `TOASTCOL_IGNORE`: the value of a `p` column, or a value in the TOAST table that the update keeps.
        ignore: bool,
    },
}

/// The values of a version after `heap_toast_insert_or_update`.
pub(super) struct Toasted {
    /// `t_len`.
    pub(super) len: usize,
    /// The number of values that `toast_save_datum` writes to the TOAST table, each with a new OID from `GetNewOidWithIndex`.
    pub(super) moved: u32,
    slots: Vec<Slot>,
}

impl Toasted {
    /// True when the value at `at` is in the TOAST table.
    fn external(&self, at: usize) -> bool {
        matches!(self.slots.get(at), Some(Slot::Varlena { held: Held::External, .. }))
    }
}

/// `heap_compute_data_size` of the values in `slots`.
fn slots_len(slots: &[Slot]) -> usize {
    slots.iter().fold(0, |len, slot| match *slot {
        Slot::Null => len,
        Slot::Fixed(size, align) => align_to(len, align) + size,
        Slot::Varlena { size, held: Held::External, .. } => len + size,
        Slot::Varlena { size, raw, held: Held::Raw, .. } if size < raw => len + size,
        Slot::Varlena { size, align, .. } => align_to(len, align) + size,
    })
}

/// The bytes of a value that the engine can compress as PostgreSQL does: the text of `text` and the bytes of `bytea`. The other types are not compressed, as if each one were incompressible.
fn plain_bytes(value: &Value) -> Option<&[u8]> {
    match value {
        Value::Text(text) => Some(text.as_bytes()),
        Value::Bytea(bytes) => Some(bytes),
        _ => None,
    }
}

/// `toast_tuple_find_biggest_attribute`: the longest value that a round of the toaster can take, with `tai_size` more than `MAXALIGN(TOAST_POINTER_SIZE)`. The first one wins a tie.
fn biggest(slots: &[Slot], storage: &[Storage], compress: bool, main: bool) -> Option<usize> {
    let mut best = None;
    let mut best_size = max_align(TOAST_POINTER);
    for (at, slot) in slots.iter().enumerate() {
        let Slot::Varlena { size, held, incompressible, ignore, .. } = *slot else { continue };
        if ignore || held == Held::External || (compress && (incompressible || held != Held::Raw)) {
            continue;
        }
        let kind = storage.get(at).copied().unwrap_or(DEFAULT_STORAGE);
        let wanted = if main {
            matches!(kind, Storage::Main(_))
        } else {
            matches!(kind, Storage::Extended(_) | Storage::External)
        };
        if wanted && size > best_size {
            best = Some(at);
            best_size = size;
        }
    }
    best
}

/// `toast_tuple_try_compression`.
fn try_compress(slot: &mut Slot, value: &Value, method: Method) {
    let Slot::Varlena { size, held, incompressible, .. } = slot else { return };
    match plain_bytes(value).and_then(|bytes| rupg_encode::toast_compress(method, bytes)) {
        Some(compressed) => {
            *size = compressed;
            *held = Held::Compressed;
        }
        None => *incompressible = true,
    }
}

/// `toast_tuple_externalize`.
fn externalize(slot: &mut Slot, moved: &mut u32) {
    if let Slot::Varlena { size, held, .. } = slot {
        *size = TOAST_POINTER;
        *held = Held::External;
        *moved += 1;
    }
}

/// `heap_form_tuple` and then `heap_toast_insert_or_update` of a version with the values `values`: the header with the null bitmap, then the data. A value of a column with a type of fixed length in `fixed` has the length and the alignment of the type. When the version is longer than `TOAST_TUPLE_THRESHOLD`, the toaster compresses its longest values and moves them to the TOAST table in four rounds, with the storage of each column in `storage`. `prior` has the values of the version that an update replaces: a value that is the same as a value of `prior` in the TOAST table stays there, and takes no new OID.
pub(super) fn toast(
    values: &[Value],
    fixed: &[Fixed],
    storage: &[Storage],
    prior: Option<&[Value]>,
) -> Toasted {
    let nulls = values.iter().any(Value::is_null);
    let header = max_align(TUPLE_HEADER + if nulls { values.len().div_ceil(8) } else { 0 });
    let kept = prior.map(|prior| (toast(prior, fixed, storage, None), prior));
    let mut slots: Vec<Slot> = values
        .iter()
        .enumerate()
        .map(|(at, value)| {
            if value.is_null() {
                return Slot::Null;
            }
            let (data, align) = match fixed
                .get(at)
                .copied()
                .flatten()
                .map_or_else(|| layout(value), |(len, align)| Layout::Fixed(len, align))
            {
                Layout::Fixed(len, align) => return Slot::Fixed(len, align),
                Layout::Varlena(data, align) => (data, align),
            };
            let kind = storage.get(at).copied().unwrap_or(DEFAULT_STORAGE);
            let same = kept.as_ref().is_some_and(|(old, prior)| {
                old.external(at) && prior.get(at).is_some_and(|p| p == value)
            });
            let short = kind != Storage::Plain && data < 127;
            Slot::Varlena {
                size: if same {
                    TOAST_POINTER
                } else if short {
                    data + 1
                } else {
                    data + 4
                },
                raw: data + 4,
                align,
                held: if same { Held::External } else { Held::Raw },
                incompressible: false,
                ignore: same || kind == Storage::Plain,
            }
        })
        .collect();
    let mut moved = 0;
    let has_external =
        slots.iter().any(|s| matches!(s, Slot::Varlena { held: Held::External, .. }));
    if header + slots_len(&slots) <= TOAST_THRESHOLD && !has_external {
        return Toasted { len: header + slots_len(&slots), moved, slots };
    }
    let target = TOAST_THRESHOLD - header;
    let method = |at: usize| match storage.get(at).copied().unwrap_or(DEFAULT_STORAGE) {
        Storage::Main(method) | Storage::Extended(method) => Some(method),
        Storage::Plain | Storage::External => None,
    };
    // Round 1: compress each value with the storage `x`, the longest first, and move a value that is still longer than the target.
    while slots_len(&slots) > target {
        let Some(at) = biggest(&slots, storage, true, false) else { break };
        match method(at) {
            Some(m) => try_compress(&mut slots[at], &values[at], m),
            None => {
                if let Slot::Varlena { incompressible, .. } = &mut slots[at] {
                    *incompressible = true;
                }
            }
        }
        if let Slot::Varlena { size, .. } = slots[at]
            && size > target
        {
            externalize(&mut slots[at], &mut moved);
        }
    }
    // Round 2: move the values with the storage `x` and `e` that are still in the version.
    while slots_len(&slots) > target {
        let Some(at) = biggest(&slots, storage, false, false) else { break };
        externalize(&mut slots[at], &mut moved);
    }
    // Round 3: compress the values with the storage `m`.
    while slots_len(&slots) > target {
        let Some(at) = biggest(&slots, storage, true, true) else { break };
        if let Some(m) = method(at) {
            try_compress(&mut slots[at], &values[at], m);
        }
    }
    // Round 4: move the values with the storage `m` only when the version does not fit in a page.
    while slots_len(&slots) > TARGET_MAIN - header {
        let Some(at) = biggest(&slots, storage, false, true) else { break };
        externalize(&mut slots[at], &mut moved);
    }
    Toasted { len: header + slots_len(&slots), moved, slots }
}

/// The length of a composite value, a `HeapTupleHeader` with its fields, which `heap_form_tuple` makes with no TOAST.
fn record_len(values: &[Value]) -> usize {
    let nulls = values.iter().any(Value::is_null);
    let header = max_align(TUPLE_HEADER + if nulls { values.len().div_ceil(8) } else { 0 });
    let layouts: Vec<Layout> = values.iter().filter(|v| !v.is_null()).map(layout).collect();
    header + data_len(&layouts)
}

/// `IndexTupleSize` of an index tuple with the values `values`, from `index_form_tuple`: the header of 8 bytes, with the null bitmap of 4 bytes up to a multiple of 8 when a value is null, and then the data, up to a multiple of 8. The values are not compressed.
pub(super) fn index_tuple_len(values: &[Value]) -> usize {
    let header = if values.iter().any(Value::is_null) { 16 } else { 8 };
    let layouts: Vec<Layout> = values.iter().filter(|v| !v.is_null()).map(layout).collect();
    max_align(header + data_len(&layouts))
}

/// The lengths of the value `value` that an expression gives, as `ANALYZE` reads them for an expression column of an index: `VARSIZE_ANY` and `toast_raw_datum_size` of a value with a header of four bytes, as the value is not in a version. A null value and a value of fixed length give 0 for both.
pub(crate) fn datum_width(value: &Value) -> (usize, usize) {
    match layout(value) {
        Layout::Varlena(size, _) if !value.is_null() => (size + 4, size + 4),
        _ => (0, 0),
    }
}

/// The lengths of the values `values` of a version, as `ANALYZE` reads them: `VARSIZE_ANY` of the value in the version, which is the length of the compressed value or of the TOAST pointer for a value that [`toast`] compresses or moves, and `toast_raw_datum_size`, the length of the value with a header of four bytes. A null value and a value of fixed length give 0 for both.
pub(crate) fn stored_widths(
    values: &[Value],
    fixed: &[Fixed],
    storage: &[Storage],
) -> Vec<(usize, usize)> {
    let toasted = toast(values, fixed, storage, None);
    (0..values.len())
        .map(|at| match toasted.slots.get(at) {
            Some(&Slot::Varlena { size, raw, .. }) => (size, raw),
            _ => (0, 0),
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
    ///
    /// `save` is the space that `fillfactor` keeps free in each page, as `RelationGetTargetPageFreeSpace` gives it. A page other than the page of the old version takes the version only when it has the space of the version and this space, or the space of a page that is nearly empty when this is less.
    pub(super) fn place(
        &mut self,
        len: usize,
        old: Option<u32>,
        bulk: Option<(u32, u32)>,
        save: usize,
    ) -> (u32, u16) {
        if self.bulk.is_some_and(|b| Some(b.made) != bulk) {
            self.bulk = None;
        }
        let free = |pages: &[Page], at: u32| pages.get(at as usize).map_or(0, Page::free);
        if let Some(at) = old.filter(|&at| free(&self.pages, at) >= len) {
            return (at, self.pages[at as usize].add(len));
        }
        let nearly_empty = MAX_TUPLE - (TUPLES_PER_PAGE as usize / 8 * usize::from(LINE_POINTER));
        let len_saved = if len + save > nearly_empty { len.max(nearly_empty) } else { len + save };
        let use_fsm = bulk.is_none();
        let count = self.blocks();
        let mut at = self.target.filter(|&at| at < count);
        if at.is_none() && use_fsm {
            at = self.fsm.get(len_saved, count);
        }
        if at.is_none() {
            at = count.checked_sub(1);
        }
        while let Some(block) = at {
            let space = free(&self.pages, block);
            if space >= len_saved {
                self.target = Some(block);
                return (block, self.pages[block as usize].add(len));
            }
            at = if let Some(b) = self.bulk.as_mut().filter(|b| b.free.0 <= b.free.1) {
                b.free.0 += 1;
                Some(b.free.0 - 1)
            } else if !use_fsm {
                None
            } else {
                self.fsm.record_and_get(block, space, len_saved, count)
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

    /// The pages with the generation `generation` of a heap that a `TRUNCATE` renewed, as the log of a commit gives it.
    pub(super) fn of_generation(generation: u16) -> Pages {
        Pages { generation, ..Pages::default() }
    }

    /// The version `tid` of `len` bytes takes its slot again, as the replay of the log of a commit puts it back: the page gets the line pointer and the space of the version. The other versions of the page keep their slots.
    pub(super) fn occupy(&mut self, tid: u64, len: usize) {
        let (at, slot) = (page_of(tid) as usize, slot_of(tid));
        if self.pages.len() <= at {
            self.pages.resize(at + 1, Page::NEW);
        }
        let page = &mut self.pages[at];
        if slot == 0 {
            return;
        }
        if slot > page.lines {
            page.lines = slot;
        } else if let Some(bits) = page.unused.as_mut() {
            let (word, bit) = (usize::from(slot - 1) / 64, (slot - 1) % 64);
            bits[word] &= !(1 << bit);
            if bits.iter().all(|&w| w == 0) {
                page.unused = None;
            }
        }
        let room = page.upper.saturating_sub(page.lower());
        page.upper -= u16::try_from(len).unwrap_or(room).min(room);
        page.visible = false;
        page.frozen = false;
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

rupg_common::persist_struct!(Pages {
    pages,
    target,
    bulk,
    fsm,
    vm_blocks,
    generation,
    relpages,
    reltuples,
    relallvisible,
    relallfrozen,
});
rupg_common::persist_struct!(Page { lines, upper, unused, visible, frozen });
rupg_common::persist_struct!(Bulk { made, free, extended_by });

/// The method of a column is its tag: 1 and 2 for `m`, 3 and 4 for `x`.
impl Persist for Storage {
    fn put(&self, out: &mut Vec<u8>) {
        let method = |m: &Method| u8::from(matches!(m, Method::Lz4));
        out.push(match self {
            Storage::Plain => 0,
            Storage::Main(m) => 1 + method(m),
            Storage::External => 5,
            Storage::Extended(m) => 3 + method(m),
        });
    }
    fn take(input: &mut Reader<'_>) -> rupg_common::Result<Self> {
        Ok(match u8::take(input)? {
            0 => Storage::Plain,
            1 => Storage::Main(Method::Pglz),
            2 => Storage::Main(Method::Lz4),
            3 => Storage::Extended(Method::Pglz),
            4 => Storage::Extended(Method::Lz4),
            5 => Storage::External,
            _ => return Err(corrupt("a storage has a bad tag")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hex digits of a linear congruential generator, which the compressors cannot make shorter.
    fn noise(len: usize) -> String {
        let mut state = 1u64;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                char::from(b"0123456789abcdef"[(state >> 60) as usize])
            })
            .collect()
    }

    #[test]
    fn toast_rounds() {
        let text = |text: String| [Value::Text(text)];
        let lz4 = [Storage::Extended(Method::Lz4)];
        // A value that does not compress moves to the TOAST table, and the tuple keeps a pointer of 18 bytes.
        let moved = toast(&text(noise(3200)), &[None], &lz4, None);
        assert_eq!((moved.moved, moved.len), (1, 24 + 18));
        // A value that compresses stays in the tuple with a header of 8 bytes.
        let compressed = toast(&text("ab".repeat(3000)), &[None], &lz4, None);
        assert_eq!((compressed.moved, compressed.len), (0, 24 + 43));
        // `main` keeps the value in the tuple while the tuple is shorter than a page.
        let main = toast(&text(noise(3200)), &[None], &[Storage::Main(Method::Lz4)], None);
        assert_eq!((main.moved, main.len), (0, 24 + 3204));
        // An update that keeps a value that is in the TOAST table uses no new OID.
        let old = text(noise(3200));
        let kept = toast(&old, &[None], &lz4, Some(&old));
        assert_eq!((kept.moved, kept.len), (0, 24 + 18));
    }
}
