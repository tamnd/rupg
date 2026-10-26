//! The size of a `btree` index, as `_bt_load` of `nbtsort.c` writes the index from the sorted versions of its table: the pages of each level, which `_bt_buildadd` fills up to the fill factor, and the height of the tree. The engine keeps no index, so the size is the size of an index that `CREATE INDEX` builds from the versions that the table has now. The pages that later inserts split are not followed.

use std::cmp::Ordering;

use rupg_types::{Value, oid};

use super::pages::{index_tuple_len, max_align};

/// `BLCKSZ`.
const BLOCK: usize = 8192;

/// `SizeOfPageHeaderData`.
const PAGE_HEADER: usize = 24;

/// `sizeof(BTPageOpaqueData)`, the special space at the end of each page.
const SPECIAL: usize = 16;

/// `sizeof(ItemIdData)`.
const LINE_POINTER: usize = 4;

/// `sizeof(IndexTupleData)`: the size of a pivot tuple with no key, the first item of an internal page.
const MINUS_INFINITY: usize = 8;

/// `sizeof(ItemPointerData)`.
const HEAP_TID: usize = 6;

/// `BTREE_NONLEAF_FILLFACTOR`.
const NONLEAF_FILLFACTOR: usize = 70;

/// `P_FIRSTKEY`: the offset of the first data item of a page that is not the rightmost page of its level.
const FIRST_KEY: u16 = 2;

/// The largest posting list tuple of `_bt_load`: `MAXALIGN_DOWN(BLCKSZ * 10 / 100)` without one line pointer.
const MAX_POSTING: usize = (BLOCK * 10 / 100) / 8 * 8 - LINE_POINTER;

/// The most versions that the size reads one by one. A larger table gives the size of a sample of this many versions for its leaf pages.
pub(super) const EXACT: usize = 100_000;

/// The parts of a `btree` index that its size depends on.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Shape {
    /// The place of each column of the index in the values of a version: the key columns, and then the `INCLUDE` columns.
    pub(crate) columns: Vec<usize>,
    /// The type of each column of the index.
    pub(crate) types: Vec<u32>,
    /// `indnkeyatts`.
    pub(crate) key_count: usize,
    /// `indoption` of each key column: 1 for `DESC` and 2 for `NULLS FIRST`.
    pub(crate) options: Vec<i16>,
    /// `indisunique`: a unique index has no posting lists after `CREATE INDEX`.
    pub(crate) unique: bool,
    /// The option `fillfactor` of the index, 90 by default.
    pub(crate) fillfactor: usize,
    /// The option `deduplicate_items` of the index, on by default.
    pub(crate) deduplicate: bool,
}

/// `RelationGetNumberOfBlocks` of an index, with the metapage, and `_bt_getrootheight`, the level of the root.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Size {
    pub(crate) blocks: u32,
    pub(crate) height: u32,
}

impl Size {
    /// The size of an index with no rows: the metapage alone.
    pub(crate) const EMPTY: Size = Size { blocks: 1, height: 0 };
}

/// The size that a table had when the engine found it, which serves while the versions of the table do not change.
#[derive(Debug)]
pub(super) struct Cached {
    pub(super) made: u64,
    pub(super) count: usize,
    pub(super) shape: Shape,
    pub(super) size: Size,
}

/// `_bt_allequalimage`: true when the operator class of each key column has a `BTEQUALIMAGE_PROC` that returns true, so that the index can have posting lists. The types of `btequalimage` and `btvarstrequalimage` with a deterministic collation have it, and `numeric`, `float4`, `float8`, `interval`, arrays and `jsonb` do not.
fn equal_image(ty: u32) -> bool {
    matches!(
        rupg_func::base_type(ty),
        oid::BOOL
            | oid::CHAR
            | oid::INT2
            | oid::INT4
            | oid::INT8
            | oid::OID
            | oid::OID8
            | oid::OIDVECTOR
            | oid::NAME
            | oid::TEXT
            | oid::VARCHAR
            | oid::BPCHAR
            | oid::BYTEA
            | oid::DATE
            | oid::TIME
            | oid::TIMETZ
            | oid::TIMESTAMP
            | oid::TIMESTAMPTZ
            | oid::UUID
            | oid::INET
            | oid::CIDR
            | oid::MACADDR
            | oid::MACADDR8
            | oid::MONEY
            | oid::TID
            | oid::PG_LSN
            | oid::XID8
            | oid::BIT
            | oid::VARBIT
    )
}

/// True when two values of a key column are equal, as the support function 1 of the column finds them. Two nulls are equal.
fn same(ty: u32, a: &Value, b: &Value) -> bool {
    match (a.is_null(), b.is_null()) {
        (true, true) => true,
        (false, false) => rupg_func::compare(ty, a, ty, b).is_ok_and(Ordering::is_eq),
        _ => false,
    }
}

impl Shape {
    /// The order of two versions in the index, as `comparetup_index_btree` gives it: each key column in the order of its `indoption`.
    fn order(&self, a: &[Value], b: &[Value]) -> Ordering {
        for at in 0..self.key_count {
            let option = self.options.get(at).copied().unwrap_or(0);
            let nulls_first = option & 2 != 0;
            let order = match (a[at].is_null(), b[at].is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) if nulls_first => Ordering::Less,
                (true, false) => Ordering::Greater,
                (false, true) if nulls_first => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    let ty = self.types[at];
                    let order =
                        rupg_func::compare(ty, &a[at], ty, &b[at]).unwrap_or(Ordering::Equal);
                    if option & 1 != 0 { order.reverse() } else { order }
                }
            };
            if order.is_ne() {
                return order;
            }
        }
        Ordering::Equal
    }

    /// `_bt_keep_natts`: the key columns that a pivot tuple between `lastleft` and `firstright` keeps, which are the equal key columns at the start and the first column that differs. One more than the key columns means that all of them are equal.
    fn keep(&self, lastleft: &[Value], firstright: &[Value]) -> usize {
        (0..self.key_count)
            .take_while(|&at| same(self.types[at], &lastleft[at], &firstright[at]))
            .count()
            + 1
    }

    /// The size of the pivot tuple of `_bt_truncate` between `lastleft` and `firstright`: the columns that it keeps, with the heap TID of `lastleft` when all the key columns are equal.
    fn pivot(&self, lastleft: &[Value], firstright: &[Value]) -> usize {
        let keep = self.keep(lastleft, firstright);
        let len = index_tuple_len(&firstright[..keep.min(self.key_count)]);
        if keep > self.key_count { len + max_align(HEAP_TID) } else { len }
    }
}

/// A page of a level that the build fills, as `BTPageState` keeps it.
#[derive(Debug)]
struct Level {
    /// `pd_lower` and `pd_upper` of the page.
    lower: usize,
    upper: usize,
    /// `btps_lastoff`: the offset of the last item of the page.
    last_off: u16,
    /// `btps_lastextra`: the space of the posting list of the last item.
    last_extra: usize,
    /// The size of the last item on the page, and the size of the pivot tuple that it gives when it moves to the next page.
    last_len: usize,
    last_pivot: usize,
    /// The size of `btps_lowkey`, the pivot tuple of the page in its parent.
    lowkey: usize,
    /// The pages of the level.
    pages: u32,
}

impl Level {
    /// `_bt_blnewpage`: an empty page, whose line pointer of the high key is already in use.
    fn new() -> Level {
        Level {
            lower: PAGE_HEADER + LINE_POINTER,
            upper: BLOCK - SPECIAL,
            last_off: 1,
            last_extra: 0,
            last_len: 0,
            last_pivot: 0,
            lowkey: 0,
            pages: 1,
        }
    }

    /// `PageGetFreeSpace`: the space of the page without one more line pointer.
    fn free(&self) -> usize {
        (self.upper - self.lower).saturating_sub(LINE_POINTER)
    }

    /// `PageAddItem` of an item of `len` bytes.
    fn push(&mut self, len: usize) {
        self.lower += LINE_POINTER;
        self.upper -= len;
    }
}

/// The levels of the tree that `_bt_load` builds, from the leaves up.
#[derive(Debug)]
struct Build {
    levels: Vec<Level>,
    /// `BTGetTargetPageFreeSpace`: a leaf page is full with less free space than this.
    leaf_full: usize,
    /// The sum and the count of the pivot tuples of the leaf pages in their parents.
    pivots: (usize, usize),
}

impl Build {
    /// `_bt_buildadd`: adds an item of `len` bytes, with `extra` bytes of posting list, to the last page of the level `at`. The item gives a pivot tuple of `pivot` bytes when it moves to the next page. When the page cannot take the item, or when the page has two items or more and is full to the fill factor, the last item of the page moves to a new page, the pivot tuple of the last item becomes the high key of the page, and the low key of the page goes to the parent. The first item of an internal page has no key.
    fn add(&mut self, at: usize, len: usize, extra: usize, pivot: usize) {
        if at == self.levels.len() {
            self.levels.push(Level::new());
        }
        let leaf = at == 0;
        let full = if leaf { self.leaf_full } else { BLOCK * (100 - NONLEAF_FILLFACTOR) / 100 };
        let level = &mut self.levels[at];
        let last_extra = std::mem::replace(&mut level.last_extra, extra);
        let space = level.free();
        let mut up = None;
        if space < len + if leaf { max_align(HEAP_TID) } else { 0 }
            || (space + last_extra < full && level.last_off > FIRST_KEY)
        {
            let moved = if leaf { level.last_len } else { MINUS_INFINITY };
            up = Some(std::mem::replace(&mut level.lowkey, level.last_pivot));
            let pages = level.pages + 1;
            let lowkey = level.lowkey;
            *level = Level { lowkey, pages, ..Level::new() };
            level.push(moved);
            level.last_off = FIRST_KEY;
        }
        if level.last_off == 1 {
            level.lowkey = MINUS_INFINITY;
        }
        level.last_off += 1;
        let stored = if !leaf && level.last_off == FIRST_KEY { MINUS_INFINITY } else { len };
        level.push(stored);
        level.last_len = stored;
        level.last_pivot = if leaf { pivot } else { stored };
        if let Some(lowkey) = up {
            self.up(at, lowkey);
        }
    }

    /// Adds the low key of a page of the level `at` to its parent.
    fn up(&mut self, at: usize, lowkey: usize) {
        if at == 0 {
            self.pivots.0 += lowkey;
            self.pivots.1 += 1;
        }
        self.add(at + 1, lowkey, 0, lowkey);
    }

    /// `_bt_uppershutdown`: the low key of the last page of each level goes to its parent, and the last level has the root.
    fn finish(mut self) -> Build {
        let mut at = 0;
        while at + 1 < self.levels.len() {
            let lowkey = self.levels[at].lowkey;
            self.up(at, lowkey);
            at += 1;
        }
        self
    }
}

/// The size of a `btree` index of the shape `shape` on a table with `total` versions, from the values of the columns of the index of the versions `entries`, in the order of the heap. The entries are sorted by the key columns, and the heap TID orders equal keys. When the index can have posting lists, the versions with equal keys share a posting list up to its largest size. With fewer entries than versions the entries are a sample: the leaf pages grow with the versions, and each internal level has the pivot tuples of the average size of the sample up to its fill factor.
pub(super) fn size(mut entries: Vec<Box<[Value]>>, total: usize, shape: &Shape) -> Size {
    if entries.is_empty() {
        return Size::EMPTY;
    }
    entries.sort_by(|a, b| shape.order(a, b));
    let dedup = shape.deduplicate
        && !shape.unique
        && shape.columns.len() == shape.key_count
        && shape.types.iter().all(|&ty| equal_image(ty));
    let mut build = Build {
        levels: Vec::new(),
        leaf_full: BLOCK * (100 - shape.fillfactor.min(100)) / 100,
        pivots: (0, 0),
    };
    let mut last: Option<usize> = None;
    let mut at = 0;
    while at < entries.len() {
        let base = index_tuple_len(&entries[at]);
        let mut end = at + 1;
        if dedup {
            while end < entries.len()
                && shape.keep(&entries[at], &entries[end]) > shape.key_count
                && max_align(base + (end - at + 1) * HEAP_TID) <= MAX_POSTING
            {
                end += 1;
            }
        }
        let (len, extra) = match end - at {
            1 => (base, 0),
            count => {
                let len = max_align(base + count * HEAP_TID);
                (len, len - base)
            }
        };
        let pivot = last.map_or(base, |l| shape.pivot(&entries[l], &entries[at]));
        build.add(0, len, extra, pivot);
        last = Some(end - 1);
        at = end;
    }
    let build = build.finish();
    if total <= entries.len() {
        let pages: u32 = build.levels.iter().map(|l| l.pages).sum();
        let height = u32::try_from(build.levels.len() - 1).unwrap_or(u32::MAX);
        return Size { blocks: pages + 1, height };
    }
    let scale = total as f64 / entries.len() as f64;
    let mut pages = (f64::from(build.levels[0].pages) * scale).ceil();
    let pivot = build.pivots.0.checked_div(build.pivots.1).unwrap_or(MINUS_INFINITY);
    let fanout =
        ((BLOCK - PAGE_HEADER - SPECIAL) * NONLEAF_FILLFACTOR / 100 / (pivot + LINE_POINTER))
            .max(3);
    let (mut blocks, mut height) = (pages + 1.0, 0);
    while pages > 1.0 {
        pages = (pages / fanout as f64).ceil();
        blocks += pages;
        height += 1;
    }
    Size { blocks: blocks.min(f64::from(u32::MAX)) as u32, height }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(
        types: Vec<u32>,
        key_count: usize,
        unique: bool,
        options: (usize, bool, bool),
    ) -> Shape {
        let (fillfactor, deduplicate, desc) = options;
        let columns = (0..types.len()).collect();
        let options = if desc { vec![3, 0] } else { vec![0; key_count] };
        Shape { columns, types, key_count, options, unique, fillfactor, deduplicate }
    }

    /// The blocks of indexes that PostgreSQL 19 builds on a table of 10000 rows, with `relpages` after `CREATE INDEX`.
    #[test]
    fn blocks_of_the_build() {
        let rows: Vec<[Value; 6]> = (1..=10000i32)
            .map(|x| {
                [
                    Value::Int4(x),
                    Value::Int4(x % 100),
                    Value::Text(format!("v{}", x % 500)),
                    if x % 3 == 0 { Value::Null } else { Value::Int4(x % 50) },
                    Value::Float8(f64::from(x % 100)),
                    Value::Int8(i64::from(x)),
                ]
            })
            .collect();
        let pick = |cols: &[usize]| -> Vec<Box<[Value]>> {
            rows.iter().map(|r| cols.iter().map(|&c| r[c].clone()).collect()).collect()
        };
        let (i4, i8, tx, f8) = (oid::INT4, oid::INT8, oid::TEXT, oid::FLOAT8);
        let plain = (90, true, false);
        let cases = [
            ("(a)", 30, vec![0], shape(vec![i4], 1, false, plain)),
            ("(b)", 11, vec![1], shape(vec![i4], 1, false, plain)),
            ("(t)", 12, vec![2], shape(vec![tx], 1, false, plain)),
            ("(n)", 11, vec![3], shape(vec![i4], 1, false, plain)),
            ("(f)", 30, vec![4], shape(vec![f8], 1, false, plain)),
            ("(b, a)", 30, vec![1, 0], shape(vec![i4, i4], 2, false, plain)),
            ("(t, b)", 12, vec![2, 1], shape(vec![tx, i4], 2, false, plain)),
            ("(c) INCLUDE (t)", 41, vec![5, 2], shape(vec![i8, tx], 1, false, plain)),
            ("(b DESC, c)", 41, vec![1, 5], shape(vec![i4, i8], 2, false, (90, true, true))),
            ("UNIQUE (a, c)", 41, vec![0, 5], shape(vec![i4, i8], 2, true, plain)),
            (
                "(c, a) fillfactor 50",
                72,
                vec![5, 0],
                shape(vec![i8, i4], 2, false, (50, true, false)),
            ),
            ("(n, t) no dedup", 39, vec![3, 2], shape(vec![i4, tx], 2, false, (90, false, false))),
        ];
        for (name, blocks, cols, s) in cases {
            assert_eq!(size(pick(&cols), 10000, &s), Size { blocks, height: 1 }, "{name}");
        }
        let texts = (1..=5000).map(|x| vec![Value::Text("w".repeat(x % 40))].into()).collect();
        assert_eq!(size(texts, 5000, &shape(vec![tx], 1, false, plain)).blocks, 7);
        let keys = (1..=60000).map(|x| vec![Value::Int8(x)].into()).collect();
        assert_eq!(size(keys, 60000, &shape(vec![i8], 1, false, plain)).blocks, 166);
        let few = (1..=60000).map(|x| vec![Value::Int4(x % 1000)].into()).collect();
        assert_eq!(size(few, 60000, &shape(vec![i4], 1, false, plain)).blocks, 52);
    }
}
