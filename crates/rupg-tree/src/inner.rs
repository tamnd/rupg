//! The inner node of a tree.
//!
//! An inner node is a 16 KiB page with the page header of spec/08 section 8.3.1. The kind data holds the level at bytes 0 and 1, the count of keys at bytes 2 and 3, and the leftmost child at bytes 8 to 15. A slot array of `u16` offsets follows the header, in key order. Each entry is at the end of the page: the child as a `u64`, the key length as a `u16`, then the key.
//!
//! The leftmost child holds the keys below the first key. The child of entry `i` holds the keys at or above key `i` and below key `i + 1`. Keys compare as bytes.

use std::cmp::Ordering;

use rupg_common::{Error, Result};
use rupg_file::{OptimisticRead, PAGE_HEADER_SIZE, PAGE_SIZE, Page, PageHeader, PageId, PageKind};

/// The longest key that a tree holds. It is the B-tree key limit of PostgreSQL (spec/12 section 12.13), and six keys of this size fit in one inner node.
pub const MAX_KEY: usize = 2704;

const LEVEL: usize = 40;
const COUNT: usize = 42;
const LEFTMOST: usize = 48;
const LOWER: usize = 34;
const UPPER: usize = 36;
const ENTRY: usize = 10;
const MAX_COUNT: usize = (PAGE_SIZE - PAGE_HEADER_SIZE) / (ENTRY + 2);

/// A key and the child that holds the keys from it.
pub(crate) type Entry = (Vec<u8>, PageId);

/// Bytes of a node, from a latched page or from an optimistic read. A read outside the page gives `None`.
pub(crate) trait View {
    fn u16_at(&self, at: usize) -> Option<u16>;
    fn u64_at(&self, at: usize) -> Option<u64>;
    fn cmp_at(&self, at: usize, len: usize, key: &[u8]) -> Option<Ordering>;
    fn bytes(&self, at: usize, len: usize) -> Option<Vec<u8>>;
}

impl View for Page {
    fn u16_at(&self, at: usize) -> Option<u16> {
        self.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn u64_at(&self, at: usize) -> Option<u64> {
        self.get(at..at + 8)?.try_into().ok().map(u64::from_le_bytes)
    }
    fn cmp_at(&self, at: usize, len: usize, key: &[u8]) -> Option<Ordering> {
        self.get(at..at.checked_add(len)?).map(|b| b.cmp(key))
    }
    fn bytes(&self, at: usize, len: usize) -> Option<Vec<u8>> {
        self.get(at..at.checked_add(len)?).map(<[u8]>::to_vec)
    }
}

/// An optimistic read.
pub(crate) struct Read<'r, R>(pub(crate) &'r R);

impl<R: OptimisticRead> View for Read<'_, R> {
    fn u16_at(&self, at: usize) -> Option<u16> {
        self.0.u16_at(at)
    }
    fn u64_at(&self, at: usize) -> Option<u64> {
        self.0.u64_at(at)
    }
    fn cmp_at(&self, at: usize, len: usize, key: &[u8]) -> Option<Ordering> {
        let mut buf = [0u8; MAX_KEY];
        let out = buf.get_mut(..len)?;
        self.0.read(at, out).then(|| (*out).cmp(key))
    }
    fn bytes(&self, at: usize, len: usize) -> Option<Vec<u8>> {
        let mut out = vec![0u8; len.min(MAX_KEY)];
        (len <= MAX_KEY && self.0.read(at, &mut out)).then_some(out)
    }
}

/// The child to follow for a key, and the lowest key that the child does not hold, if the node knows it.
#[derive(Debug)]
pub(crate) struct Step {
    pub(crate) child: PageId,
    pub(crate) fence: Option<Vec<u8>>,
}

fn count(v: &impl View) -> Option<usize> {
    let n = usize::from(v.u16_at(COUNT)?);
    (n <= MAX_COUNT).then_some(n)
}

/// The offset and length of the key of entry `i`.
fn key_of(v: &impl View, i: usize) -> Option<(usize, usize)> {
    let at = usize::from(v.u16_at(PAGE_HEADER_SIZE + 2 * i)?);
    let len = usize::from(v.u16_at(at + 8)?);
    (at >= PAGE_HEADER_SIZE && len <= MAX_KEY && at + ENTRY + len <= PAGE_SIZE)
        .then_some((at + ENTRY, len))
}

fn child_of(v: &impl View, i: usize) -> Option<PageId> {
    let at = usize::from(v.u16_at(PAGE_HEADER_SIZE + 2 * i)?);
    v.u64_at(at).map(PageId)
}

/// The step for `key`. `None` means that the node bytes are not valid. In an optimistic read, the caller then checks the version.
pub(crate) fn step(v: &impl View, key: &[u8]) -> Option<Step> {
    let n = count(v)?;
    // The number of keys at or below `key`.
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let (at, len) = key_of(v, mid)?;
        if v.cmp_at(at, len, key)? == Ordering::Greater {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let child = if lo == 0 { PageId(v.u64_at(LEFTMOST)?) } else { child_of(v, lo - 1)? };
    let fence = match lo < n {
        true => {
            let (at, len) = key_of(v, lo)?;
            Some(v.bytes(at, len)?)
        }
        false => None,
    };
    Some(Step { child, fence })
}

/// The level of a node. Leaves are level 0.
pub(crate) fn level(page: &Page) -> u16 {
    page.u16_at(LEVEL).unwrap_or(0)
}

/// The leftmost child.
pub(crate) fn leftmost(page: &Page) -> PageId {
    PageId(page.u64_at(LEFTMOST).unwrap_or(0))
}

/// True if an entry with a key of `len` bytes fits.
pub(crate) fn fits(page: &Page, len: usize) -> bool {
    let (lower, upper) = (page.u16_at(LOWER).unwrap_or(0), page.u16_at(UPPER).unwrap_or(0));
    usize::from(upper).saturating_sub(usize::from(lower)) >= ENTRY + len + 2
}

/// The entries in key order. Bytes that are not valid give SQLSTATE `XX001`.
pub(crate) fn entries(page: &Page) -> Result<Vec<Entry>> {
    let bad = || Error::corrupted(format!("the inner node {} is not valid", id(page)));
    let n = count(page).ok_or_else(bad)?;
    let mut out: Vec<Entry> = Vec::with_capacity(n);
    for i in 0..n {
        let (at, len) = key_of(page, i).ok_or_else(bad)?;
        let key = page[at..at + len].to_vec();
        if out.last().is_some_and(|(last, _)| *last >= key) {
            return Err(bad());
        }
        out.push((key, child_of(page, i).ok_or_else(bad)?));
    }
    Ok(out)
}

/// The logical page number in the page header.
pub(crate) fn id(page: &Page) -> PageId {
    PageId(page.u64_at(16).unwrap_or(0))
}

/// Writes a node with `entries`, which must be in key order and must fit.
pub(crate) fn build(
    page: &mut Page,
    id: PageId,
    kind: PageKind,
    level: u16,
    leftmost: PageId,
    entries: &[Entry],
) -> Result<()> {
    let need: usize = entries.iter().map(|(k, _)| ENTRY + k.len() + 2).sum();
    if need > PAGE_SIZE - PAGE_HEADER_SIZE || entries.iter().any(|(k, _)| k.len() > MAX_KEY) {
        return Err(Error::internal(format!(
            "{} entries do not fit in an inner node",
            entries.len()
        )));
    }
    page.fill(0);
    let mut header = PageHeader::new(id, kind);
    header.kind_data[0..2].copy_from_slice(&level.to_le_bytes());
    header.kind_data[2..4].copy_from_slice(&(entries.len() as u16).to_le_bytes());
    header.kind_data[8..16].copy_from_slice(&leftmost.0.to_le_bytes());
    let mut upper = PAGE_SIZE;
    for (i, (key, child)) in entries.iter().enumerate() {
        upper -= ENTRY + key.len();
        page[upper..upper + 8].copy_from_slice(&child.0.to_le_bytes());
        page[upper + 8..upper + 10].copy_from_slice(&(key.len() as u16).to_le_bytes());
        page[upper + ENTRY..upper + ENTRY + key.len()].copy_from_slice(key);
        let slot = PAGE_HEADER_SIZE + 2 * i;
        page[slot..slot + 2].copy_from_slice(&(upper as u16).to_le_bytes());
    }
    header.lower = (PAGE_HEADER_SIZE + 2 * entries.len()) as u16;
    header.upper = upper as u16;
    header.write(page);
    Ok(())
}

/// Cuts the entries of a full node into a left part, the entry that goes up to the parent, and a right part. The cut is at the middle of the bytes and not at the middle of the count, so each part fits when the keys have different lengths. `all` must hold at least two entries.
pub(crate) fn halves(mut all: Vec<Entry>) -> (Vec<Entry>, Entry, Vec<Entry>) {
    let size = |e: &Entry| ENTRY + e.0.len() + 2;
    let total: usize = all.iter().map(size).sum();
    let mut acc = 0;
    let mut middle = all.len() - 1;
    for (i, e) in all.iter().enumerate() {
        acc += size(e);
        if acc * 2 > total {
            middle = i;
            break;
        }
    }
    let right = all.split_off(middle + 1);
    let up = all.pop().unwrap_or_default();
    (all, up, right)
}

/// Adds an entry. The entry must fit, and its key must not be in the node.
pub(crate) fn insert(page: &mut Page, key: &[u8], child: PageId) -> Result<()> {
    let n = count(page).ok_or_else(|| Error::corrupted("an inner node has a bad count"))?;
    if !fits(page, key.len()) || key.len() > MAX_KEY {
        return Err(Error::internal("the key does not fit in the inner node"));
    }
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let (at, len) = key_of(page, mid).ok_or_else(|| Error::corrupted("bad inner node"))?;
        match page[at..at + len].cmp(key) {
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
            Ordering::Equal => return Err(Error::internal("the key is in the inner node")),
        }
    }
    let upper = usize::from(page.u16_at(UPPER).unwrap_or(0)) - ENTRY - key.len();
    page[upper..upper + 8].copy_from_slice(&child.0.to_le_bytes());
    page[upper + 8..upper + 10].copy_from_slice(&(key.len() as u16).to_le_bytes());
    page[upper + ENTRY..upper + ENTRY + key.len()].copy_from_slice(key);
    let slots = PAGE_HEADER_SIZE + 2 * lo;
    page.copy_within(slots..PAGE_HEADER_SIZE + 2 * n, slots + 2);
    page[slots..slots + 2].copy_from_slice(&(upper as u16).to_le_bytes());
    page[COUNT..COUNT + 2].copy_from_slice(&((n + 1) as u16).to_le_bytes());
    page[LOWER..LOWER + 2]
        .copy_from_slice(&((PAGE_HEADER_SIZE + 2 * (n + 1)) as u16).to_le_bytes());
    page[UPPER..UPPER + 2].copy_from_slice(&(upper as u16).to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(entries: &[Entry]) -> Box<Page> {
        let mut page = Box::new([0u8; PAGE_SIZE]);
        build(&mut page, PageId(9), PageKind::HotInner, 1, PageId(100), entries).unwrap();
        page
    }

    #[test]
    fn steps() {
        let e: Vec<Entry> = vec![(b"b".to_vec(), PageId(101)), (b"d".to_vec(), PageId(102))];
        let page = node(&e);
        assert_eq!((id(&page), level(&page), leftmost(&page)), (PageId(9), 1, PageId(100)));
        assert_eq!(PageHeader::read(&page).unwrap().kind, PageKind::HotInner);
        let s = |k: &[u8]| {
            let s = step(&*page, k).unwrap();
            (s.child.0, s.fence)
        };
        assert_eq!(s(b"a"), (100, Some(b"b".to_vec())));
        assert_eq!(s(b"b"), (101, Some(b"d".to_vec())));
        assert_eq!(s(b"c"), (101, Some(b"d".to_vec())));
        assert_eq!(s(b"d"), (102, None));
        assert_eq!(s(b"zz"), (102, None));
        assert_eq!(entries(&page).unwrap(), e);
    }

    #[test]
    fn insert_until_full() {
        let mut page = node(&[]);
        assert_eq!(step(&*page, b"x").unwrap().child, PageId(100));
        let mut keys = Vec::new();
        let mut n = 0u64;
        // Keys of 200 bytes in a mixed order.
        while fits(&page, 200) {
            let k = (n * 7919 % 1000).to_be_bytes().repeat(25);
            insert(&mut page, &k, PageId(1000 + n)).unwrap();
            keys.push((k, PageId(1000 + n)));
            n += 1;
        }
        assert_eq!(n as usize, (PAGE_SIZE - PAGE_HEADER_SIZE) / (ENTRY + 200 + 2));
        keys.sort();
        assert_eq!(entries(&page).unwrap(), keys);
        assert!(insert(&mut page, &[1; 200], PageId(1)).is_err());
        // A rebuild from the entries gives the same answers.
        let again = node(&keys);
        for (k, c) in &keys {
            assert_eq!(step(&*again, k).unwrap().child, *c);
            assert_eq!(step(&*page, k).unwrap().child, *c);
        }
        let mut small = node(&keys[..2]);
        assert!(insert(&mut small, &keys[1].0, PageId(5)).is_err());
    }

    #[test]
    fn halves_fit() {
        // Large keys at the start and small keys at the end. A cut at the middle of the count would put too many large keys on the left.
        let mut all: Vec<Entry> =
            (0..8u8).map(|i| (vec![i; MAX_KEY], PageId(u64::from(i)))).collect();
        all.extend((0..40u8).map(|i| (vec![9, i], PageId(100 + u64::from(i)))));
        let (left, up, right) = halves(all.clone());
        assert_eq!(left.len() + 1 + right.len(), all.len());
        assert_eq!(up, all[left.len()]);
        let mut p = Box::new([0u8; PAGE_SIZE]);
        build(&mut p, PageId(1), PageKind::HotInner, 1, PageId(2), &left).unwrap();
        build(&mut p, PageId(1), PageKind::HotInner, 1, up.1, &right).unwrap();
        let (left, up, right) = halves(all[..2].to_vec());
        assert_eq!((left.len(), up, right.len()), (1, all[1].clone(), 0));
    }

    #[test]
    fn bad_bytes() {
        let mut page = node(&[(b"b".to_vec(), PageId(101))]);
        page[COUNT..COUNT + 2].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(step(&*page, b"a").is_none());
        assert_eq!(entries(&page).unwrap_err().state(), rupg_common::SqlState::DATA_CORRUPTED);
        let mut page = node(&[(b"b".to_vec(), PageId(101))]);
        page[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + 2]
            .copy_from_slice(&(PAGE_SIZE as u16 - 4).to_le_bytes());
        assert!(step(&*page, b"a").is_none());
        let long = vec![(vec![0u8; MAX_KEY + 1], PageId(1))];
        let mut p = Box::new([0u8; PAGE_SIZE]);
        assert!(build(&mut p, PageId(1), PageKind::HotInner, 1, PageId(2), &long).is_err());
    }
}
