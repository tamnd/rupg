//! The pages of a new relation in PostgreSQL. rupg keeps no files, so the catalog gives the pages that the files of PostgreSQL have when the relation is new.
//!
//! This is a port of `get_typavgwidth` of `lsyscache.c`, the estimate of `table_block_relation_estimate_size` of `tableam.c` for a table that was never vacuumed, and the count of buckets of `_hash_init_metabuffer` of `hashpage.c` with `_hash_spareindex` and `_hash_get_totalbuckets` of `hashutil.c`.

use crate::toast::type_maximum_size;

/// `BLCKSZ`.
const BLOCK: i64 = 8192;
/// `SizeOfPageHeaderData`.
const PAGE_HEADER: i64 = 24;
/// `HEAP_OVERHEAD_BYTES_PER_TUPLE`: `MAXALIGN(SizeofHeapTupleHeader) + sizeof(ItemIdData)`.
const HEAP_OVERHEAD: i64 = 28;
/// The pages that the estimate uses for a table that was never vacuumed.
const MIN_PAGES: f64 = 10.0;
/// `HASH_DEFAULT_FILLFACTOR`.
const HASH_FILLFACTOR: i64 = 75;
/// The bytes of one item of a hash index: `MAXALIGN(sizeof(IndexTupleData)) + MAXALIGN(sizeof(uint32)) + sizeof(ItemIdData)`.
const HASH_ITEM: i64 = 20;
/// The most buckets of a new hash index.
const HASH_MAX_BUCKETS: u32 = 0x4000_0000;
/// `HASH_SPLITPOINT_GROUPS_WITH_ONE_PHASE`.
const ONE_PHASE_GROUPS: u32 = 10;
/// `HASH_SPLITPOINT_PHASE_BITS`.
const PHASE_BITS: u32 = 2;
/// `HASH_SPLITPOINT_PHASE_MASK`.
const PHASE_MASK: u32 = (1 << PHASE_BITS) - 1;

/// The OID of `bpchar`.
const BPCHAR: u32 = 1042;
/// The OID of the access method `hash`.
const HASH_AM: u32 = 405;
/// The OID of the access method `gin`.
const GIN_AM: u32 = 2742;
/// The OID of the access method `spgist`.
const SPGIST_AM: u32 = 4000;
/// The OID of the access method `brin`.
const BRIN_AM: u32 = 3580;

/// `get_typavgwidth`: the average width of a value of a type with the length `len` and the modifier `typmod`.
pub fn typavgwidth(len: i16, ty: u32, typmod: i32) -> i32 {
    if len > 0 {
        return i32::from(len);
    }
    match type_maximum_size(ty, typmod) {
        Some(max) if max > 0 => {
            if ty == BPCHAR || max <= 32 {
                max
            } else if max < 1000 {
                32 + (max - 32) / 2
            } else {
                32 + (1000 - 32) / 2
            }
        }
        _ => 32,
    }
}

/// The rows that PostgreSQL estimates for a table that was never vacuumed: ten pages full of rows with the average widths of the columns, and `fillfactor` percent of each page in use.
pub fn new_table_tuples(widths: impl IntoIterator<Item = i32>, fillfactor: i32) -> f64 {
    let width = widths.into_iter().map(i64::from).sum::<i64>() + HEAP_OVERHEAD;
    let density = (BLOCK - PAGE_HEADER) * i64::from(fillfactor) / 100 / width;
    (density.max(1) as f64 * MIN_PAGES).round()
}

/// The pages of the main fork of a new index of an access method on a table with an estimate of `tuples` rows, as its `ambuild` writes them: the metapage and the root of `btree`, `gist` and `gin`, the metapage, the bucket pages and the bitmap of `hash`, the metapage and the two roots of `spgist`, and the metapage, the range map and the first regular page of `brin`.
pub fn empty_index_pages(method: u32, tuples: f64) -> i32 {
    match method {
        HASH_AM => i32::try_from(hash_buckets(tuples) + 2).unwrap_or(i32::MAX),
        GIN_AM => 2,
        SPGIST_AM | BRIN_AM => 3,
        _ => 1,
    }
}

/// The buckets of a new hash index: the buckets for the fill factor and the estimate of the rows, rounded up to the buckets of a split point, and at least 2.
fn hash_buckets(tuples: f64) -> u32 {
    let ffactor = (BLOCK * HASH_FILLFACTOR / 100 / HASH_ITEM).max(10);
    let buckets = tuples / ffactor as f64;
    if buckets <= 2.0 {
        2
    } else if buckets >= f64::from(HASH_MAX_BUCKETS) {
        HASH_MAX_BUCKETS
    } else {
        total_buckets(spare_index(buckets as u32))
    }
}

/// `_hash_spareindex`: the split point phase of a count of buckets.
fn spare_index(buckets: u32) -> u32 {
    let group = if buckets < 2 { 0 } else { 32 - (buckets - 1).leading_zeros() };
    if group < ONE_PHASE_GROUPS {
        return group;
    }
    ONE_PHASE_GROUPS
        + ((group - ONE_PHASE_GROUPS) << PHASE_BITS)
        + (((buckets - 1) >> (group - (PHASE_BITS + 1))) & PHASE_MASK)
}

/// `_hash_get_totalbuckets`: the buckets that exist after a split point phase.
fn total_buckets(phase: u32) -> u32 {
    if phase < ONE_PHASE_GROUPS {
        return 1 << phase;
    }
    let group = ONE_PHASE_GROUPS + ((phase - ONE_PHASE_GROUPS) >> PHASE_BITS);
    let within = ((phase - ONE_PHASE_GROUPS) & PHASE_MASK) + 1;
    (1 << (group - 1)) + ((1 << (group - 1)) >> PHASE_BITS) * within
}
