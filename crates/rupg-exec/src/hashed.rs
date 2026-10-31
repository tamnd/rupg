//! The order of the groups of the hash table of a `HashAggregate` or a `SetOp`, as `TupleHashTable` of `execGrouping.c` keeps them in a table of `simplehash.h`. The size of the table comes from `numGroups` of the plan. The hash of a row combines the hash of each key and goes through `murmurhash32`. An entry goes into the table with robin hood hashing, and the table grows when it is full. A scan of the table starts at the first empty bucket and goes back from it.

use rupg_common::Result;
use rupg_types::Value;

/// `SH_FILLFACTOR`.
const FILLFACTOR: f64 = 0.9;
/// `SH_MAX_FILLFACTOR`.
const MAX_FILLFACTOR: f64 = 0.98;
/// `SH_GROW_MAX_DIB`: the distance from the optimal bucket that makes the table grow.
const GROW_MAX_DIB: u64 = 25;
/// `SH_GROW_MAX_MOVE`: the number of entries to move that makes the table grow.
const GROW_MAX_MOVE: u64 = 150;
/// `SH_GROW_MIN_FILLFACTOR`: a table that is less full than this does not grow for a long run.
const GROW_MIN_FILLFACTOR: f64 = 0.1;
/// `SH_MAX_SIZE`.
const MAX_SIZE: u64 = 1 << 32;
/// `get_hash_memory_limit` with the default `work_mem` and `hash_mem_multiplier`.
const HASH_MEM: f64 = 8.0 * 1024.0 * 1024.0;
/// About the size of an entry of the table with its row, which limits the first size of the table.
const ENTRY_SIZE: f64 = 64.0;
/// A bucket with no entry.
const EMPTY: u32 = u32::MAX;

/// `murmurhash32`.
fn murmurhash32(data: u32) -> u32 {
    let mut h = data;
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h
}

/// The hash of the keys of a row, as `TupleHashTableHash` gives it: the hash of each key of the type, with a rotation of one bit before each key. A null key adds nothing.
///
/// # Errors
///
/// `42883` for a type with no hash function.
pub(crate) fn hash<'v>(keys: impl IntoIterator<Item = (u32, &'v Value)>) -> Result<u32> {
    let mut hash: u32 = 0;
    for (ty, value) in keys {
        hash = hash.rotate_left(1);
        if !value.is_null() {
            #[allow(clippy::cast_possible_truncation)]
            let one = rupg_func::extended_hash(ty, value, 0)? as u32;
            hash ^= one;
        }
    }
    Ok(murmurhash32(hash))
}

/// The number of entries that the table is made for, from `numGroups`: `hash_choose_num_buckets`, which keeps the table in half of `hash_mem`.
pub(crate) fn buckets(groups: f64) -> u64 {
    let most = (HASH_MEM / ENTRY_SIZE) / 2.0;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let wanted = groups.min(most) as u64;
    wanted.max(1)
}

/// A hash table of `simplehash.h` with the hash of each entry. The entries are numbered in the order that they go into the table.
pub(crate) struct Table {
    /// The entry of each bucket, or [`EMPTY`].
    buckets: Vec<u32>,
    /// The hash of each entry.
    hashes: Vec<u32>,
    /// `grow_threshold`.
    grow_threshold: u64,
}

impl Table {
    /// `SH_CREATE` for `elements` entries.
    pub(crate) fn new(elements: u64) -> Table {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let size = ((elements as f64 / FILLFACTOR) as u64).min(MAX_SIZE);
        let mut table = Table { buckets: Vec::new(), hashes: Vec::new(), grow_threshold: 0 };
        table.resize(size);
        table
    }

    /// `SH_COMPUTE_PARAMETERS`: an empty table of `size` buckets, a power of two.
    fn resize(&mut self, size: u64) {
        let size = size.max(2).next_power_of_two().min(MAX_SIZE);
        let factor = if size == MAX_SIZE { MAX_FILLFACTOR } else { FILLFACTOR };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        {
            self.grow_threshold = (size as f64 * factor) as u64;
            self.buckets = vec![EMPTY; size as usize];
        }
    }

    fn mask(&self) -> usize {
        self.buckets.len() - 1
    }

    /// The bucket of a hash with no collision.
    fn optimal(&self, hash: u32) -> usize {
        hash as usize & self.mask()
    }

    /// `SH_DISTANCE_FROM_OPTIMAL`.
    fn distance(&self, optimal: usize, bucket: usize) -> u64 {
        let size = self.buckets.len();
        (if optimal <= bucket { bucket - optimal } else { size + bucket - optimal }) as u64
    }

    fn members(&self) -> u64 {
        self.hashes.len() as u64
    }

    /// True when the table is full enough to grow for a long run.
    #[allow(clippy::cast_precision_loss)]
    fn full_enough(&self) -> bool {
        self.members() as f64 / self.buckets.len() as f64 >= GROW_MIN_FILLFACTOR
    }

    /// `SH_GROW`: a table of `size` buckets with the entries of the table. The copy starts at the first bucket that is empty or has an entry at its optimal bucket in the new table, and puts each entry in the first empty bucket from its optimal one.
    fn grow(&mut self, size: u64) {
        let old = std::mem::take(&mut self.buckets);
        self.resize(size);
        let start = old
            .iter()
            .enumerate()
            .position(|(i, &e)| e == EMPTY || self.optimal(self.hashes[e as usize]) == i)
            .unwrap_or(0);
        for i in 0..old.len() {
            let entry = old[(start + i) % old.len()];
            if entry == EMPTY {
                continue;
            }
            let mut bucket = self.optimal(self.hashes[entry as usize]);
            while self.buckets[bucket] != EMPTY {
                bucket = (bucket + 1) & self.mask();
            }
            self.buckets[bucket] = entry;
        }
    }

    /// `SH_INSERT_HASH`: the entry of the table that `equal` finds equal to the row, with true, or else a new entry for the row, with false. `equal` compares the row with an entry of the same hash.
    pub(crate) fn insert(
        &mut self,
        hash: u32,
        equal: &mut dyn FnMut(u32) -> Result<bool>,
    ) -> Result<(u32, bool)> {
        'restart: loop {
            // The check comes first also when the row has an entry.
            if self.members() >= self.grow_threshold {
                self.grow(self.buckets.len() as u64 * 2);
            }
            let mask = self.mask();
            let mut bucket = self.optimal(hash);
            let mut insert_distance = 0;
            loop {
                let entry = self.buckets[bucket];
                if entry == EMPTY {
                    return Ok((self.add(bucket, hash), false));
                }
                if self.hashes[entry as usize] == hash && equal(entry)? {
                    return Ok((entry, true));
                }
                let distance = self.distance(self.optimal(self.hashes[entry as usize]), bucket);
                if insert_distance > distance {
                    // The entries from this bucket to the next empty bucket move one bucket forward.
                    let mut empty = bucket;
                    let mut moves = 0;
                    loop {
                        empty = (empty + 1) & mask;
                        if self.buckets[empty] == EMPTY {
                            break;
                        }
                        moves += 1;
                        if moves > GROW_MAX_MOVE && self.full_enough() {
                            self.grow_threshold = 0;
                            continue 'restart;
                        }
                    }
                    while empty != bucket {
                        let before = (empty + mask) & mask;
                        self.buckets[empty] = self.buckets[before];
                        empty = before;
                    }
                    return Ok((self.add(bucket, hash), false));
                }
                bucket = (bucket + 1) & mask;
                insert_distance += 1;
                if insert_distance > GROW_MAX_DIB && self.full_enough() {
                    self.grow_threshold = 0;
                    continue 'restart;
                }
            }
        }
    }

    /// A new entry with the hash in the bucket.
    fn add(&mut self, bucket: usize, hash: u32) -> u32 {
        #[allow(clippy::cast_possible_truncation)]
        let entry = self.hashes.len() as u32;
        self.hashes.push(hash);
        self.buckets[bucket] = entry;
        entry
    }

    /// `SH_LOOKUP_HASH`: the entry of the table that `equal` finds equal to the row, or `None`.
    pub(crate) fn find(
        &self,
        hash: u32,
        equal: &mut dyn FnMut(u32) -> Result<bool>,
    ) -> Result<Option<u32>> {
        let mut bucket = self.optimal(hash);
        loop {
            let entry = self.buckets[bucket];
            if entry == EMPTY {
                return Ok(None);
            }
            if self.hashes[entry as usize] == hash && equal(entry)? {
                return Ok(Some(entry));
            }
            bucket = (bucket + 1) & self.mask();
        }
    }

    /// `SH_START_ITERATE` and `SH_ITERATE`: the entries in the order of a scan of the table, from the first empty bucket back to it.
    pub(crate) fn order(&self) -> Vec<u32> {
        let mask = self.mask();
        let start = self.buckets.iter().position(|&e| e == EMPTY).unwrap_or(0);
        let mut out = Vec::with_capacity(self.hashes.len());
        let mut bucket = start;
        loop {
            let entry = self.buckets[bucket];
            bucket = (bucket + mask) & mask;
            if entry != EMPTY {
                out.push(entry);
            }
            if bucket == start {
                return out;
            }
        }
    }
}

/// The groups of `count` rows in the order of a hash table for `groups` groups: the rows of each group in their order. `hash` gives the hash of a row, and `equal` compares two rows of the same hash.
pub(crate) fn groups(
    count: usize,
    groups: f64,
    hash: &mut dyn FnMut(usize) -> Result<u32>,
    equal: &mut dyn FnMut(usize, usize) -> Result<bool>,
) -> Result<Vec<Vec<usize>>> {
    let mut table = Table::new(buckets(groups));
    let mut found: Vec<Vec<usize>> = Vec::new();
    for row in 0..count {
        let h = hash(row)?;
        let (entry, old) = table.insert(h, &mut |e| equal(found[e as usize][0], row))?;
        if old {
            found[entry as usize].push(row);
        } else {
            found.push(vec![row]);
        }
    }
    let mut out: Vec<Vec<usize>> = Vec::with_capacity(found.len());
    let mut found: Vec<Option<Vec<usize>>> = found.into_iter().map(Some).collect();
    for entry in table.order() {
        out.extend(found[entry as usize].take());
    }
    Ok(out)
}

/// The order of the entries of a hash table for `groups` groups, for the rows of entries whose hashes are `hashes`, in the order of their first rows. `trailing` is true when a row of an entry came after the first row of the last entry: the table checks its size before each row, also before a row of an entry that it has.
///
/// # Errors
///
/// None in practice: the entries are never equal.
pub(crate) fn order_of(hashes: &[u32], groups: f64, trailing: bool) -> Result<Vec<usize>> {
    let mut table = Table::new(buckets(groups));
    for &h in hashes {
        table.insert(h, &mut |_| Ok(false))?;
    }
    if trailing && let Some(&last) = hashes.last() {
        #[allow(clippy::cast_possible_truncation)]
        let entry = (hashes.len() - 1) as u32;
        table.insert(last, &mut |e| Ok(e == entry))?;
    }
    Ok(table.order().into_iter().map(|e| e as usize).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The order of the entries of a table for `groups` groups after the hashes go in.
    fn order(groups: f64, hashes: &[u32]) -> Vec<u32> {
        let mut table = Table::new(buckets(groups));
        for &h in hashes {
            table.insert(h, &mut |_| Ok(false)).unwrap();
        }
        table.order()
    }

    #[test]
    fn murmurhash32_is_the_hash_of_postgresql() {
        assert_eq!(murmurhash32(0), 0);
        assert_eq!(murmurhash32(1), 0x514e_28b7);
    }

    #[test]
    fn a_scan_starts_at_the_first_empty_bucket_and_goes_back() {
        // 4 groups make a table of 4 buckets. The hashes 1 and 2 go to the buckets 1 and 2, and 6 goes to the bucket 3 after 2. The scan starts at the bucket 0.
        assert_eq!(order(4.0, &[1, 2, 6]), vec![2, 1, 0]);
        // The hash 3 takes the bucket 0 from the hash 0, which is nearer to its bucket and moves to the bucket 1. The scan starts at the empty bucket 2.
        assert_eq!(order(4.0, &[0, 7, 3]), vec![0, 2, 1]);
    }

    #[test]
    fn robin_hood_moves_an_entry_that_is_near_its_bucket() {
        // The hashes 0 and 8 have the bucket 0, so 8 goes to the bucket 1, and 1 goes to the bucket 2. The hash 16 also has the bucket 0. At the bucket 2 it is farther from its bucket than 1 is, so it takes the bucket 2, and 1 moves to the bucket 3.
        let mut table = Table::new(7);
        for h in [0, 8, 1, 16] {
            table.insert(h, &mut |_| Ok(false)).unwrap();
        }
        assert_eq!(table.buckets[..4], [0, 1, 3, 2]);
    }

    #[test]
    fn a_full_table_grows() {
        // A table of 2 buckets grows before its second entry, as the threshold is 1.
        let mut table = Table::new(1);
        assert_eq!(table.buckets.len(), 2);
        for h in [0, 1, 2] {
            table.insert(h, &mut |_| Ok(false)).unwrap();
        }
        assert_eq!(table.buckets.len(), 4);
        assert_eq!(table.order(), vec![2, 1, 0]);
    }

    #[test]
    fn rows_with_equal_keys_make_one_group() {
        let keys = [5, 7, 5, 9, 7];
        let groups =
            groups(keys.len(), 3.0, &mut |row| Ok(murmurhash32(keys[row])), &mut |a, b| {
                Ok(keys[a] == keys[b])
            })
            .unwrap();
        let mut sorted = groups.clone();
        sorted.sort();
        assert_eq!(sorted, vec![vec![0, 2], vec![1, 4], vec![3]]);
    }
}
