//! The tree: descent with optimistic latch coupling, leaf writes, splits and scans.
//!
//! A reader or a writer goes down the tree with optimistic reads and takes a latch only on the leaf. After it takes the latch, it checks that the parent did not change. A writer whose leaf is full goes down again from the root with exclusive latches. It releases the latches above an inner node that has room for one more entry, and then splits the leaf and the full nodes above it. Each split holds the latch on the parent, so the parent check finds every split that a reader can miss. This is the optimistic lock coupling of Leis, Haubenschild and Neumann (IEEE Data Eng. Bull. 42, 2019).
//!
//! The root page does not move. A root split copies the root to a new page, splits that page, and writes the root again as an inner node one level up. At M1 the tree has no merges, so a separator stays in the tree after it goes in. A scan uses this: it goes down again at the upper bound of each leaf and needs no sibling links.

use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;

use rupg_common::{Error, Result};
use rupg_file::{OptimisticRead, Page, PageAccess, PageHeader, PageId, PageKind};

use crate::inner::{self, Entry, MAX_KEY, Read};

/// The offset of the kind in the page header.
const KIND: usize = 30;

/// The most splits for one write. A leaf format that cannot take a key after this many splits has a bug.
const MAX_SPLITS: usize = 32;

/// The leaf format of a tree. The tree holds the inner nodes. Each user of the tree, for example the hot store or the catalog, gives its own leaf.
///
/// A leaf must not hold its own page number outside the page header, because a root split copies the root leaf to a new page.
pub trait Leaf {
    /// The page kind of the inner nodes.
    const INNER: PageKind;
    /// The page kind of the leaves.
    const LEAF: PageKind;

    /// Writes an empty leaf with the page header for `id`.
    fn init(page: &mut Page, id: PageId);

    /// Moves the upper keys of `left` to `right`, which is an empty leaf from [`Leaf::init`]. `key` is the key that did not fit. The result is the separator: each key in `left` is below it, and each key in `right` is at or above it. `left` must keep at least one key, or else each key that moves must be above `key`, so that the separator is above the lower bound of the leaf. The separator must not be longer than [`MAX_KEY`].
    fn split(left: &mut Page, right: &mut Page, key: &[u8]) -> Result<Vec<u8>>;

    /// The lowest and the highest key in the leaf, or `None` for an empty leaf. [`Tree::check`] uses it.
    fn bounds(page: &Page) -> Result<Option<(Vec<u8>, Vec<u8>)>>;
}

/// The result of a leaf write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write<T> {
    /// The write is done.
    Done(T),
    /// The leaf has no room. The write must not change the page when it gives this. The tree splits the leaf and calls the write again.
    Full,
}

/// The shape of a tree, from [`Tree::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Shape {
    /// The number of levels. A tree with only a root leaf has depth 1.
    pub depth: u16,
    /// The number of inner nodes.
    pub inner: u64,
    /// The number of leaves.
    pub leaves: u64,
}

/// The state of [`Tree::check_with`].
struct Walk<'a> {
    shape: Shape,
    visit: &'a mut dyn FnMut(PageId, &Page) -> Result<()>,
}

/// A B+ tree with the leaf format `L`. The value is only the root page number, so it is cheap to copy. The pages are given to each call.
pub struct Tree<L> {
    root: PageId,
    leaf: PhantomData<fn() -> L>,
}

impl<L> Clone for Tree<L> {
    fn clone(&self) -> Tree<L> {
        *self
    }
}

impl<L> Copy for Tree<L> {}

impl<L> fmt::Debug for Tree<L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tree").field("root", &self.root).finish()
    }
}

fn corrupt(page: PageId, what: &str) -> Error {
    Error::corrupted(format!("the tree page {page} {what}"))
}

impl<L: Leaf> Tree<L> {
    /// Makes a tree with one empty leaf as the root.
    pub fn create<P: PageAccess>(pages: &P) -> Result<Tree<L>> {
        let (root, mut page) = pages.allocate()?;
        L::init(&mut page, root);
        Ok(Tree::open(root))
    }

    /// The tree with the root page `root`.
    pub fn open(root: PageId) -> Tree<L> {
        Tree { root, leaf: PhantomData }
    }

    /// The root page. It does not change.
    pub fn root(&self) -> PageId {
        self.root
    }

    /// Goes down to the leaf for `key` and latches it with `latch`. The result also holds the upper bound of the leaf, or `None` for the last leaf.
    fn find<'a, P: PageAccess, G: Deref<Target = Page>>(
        &self,
        pages: &'a P,
        key: &[u8],
        latch: impl Fn(&'a P, PageId) -> Result<G>,
    ) -> Result<(G, Option<Vec<u8>>)> {
        'restart: loop {
            let mut parent: Option<P::Optimistic<'a>> = None;
            let mut node = pages.optimistic(self.root)?;
            let mut fence: Option<Vec<u8>> = None;
            let mut above = u16::MAX;
            loop {
                let kind = node.u8_at(KIND);
                if kind == Some(L::LEAF as u8) {
                    let guard = latch(pages, node.page())?;
                    // A split of the leaf holds the latch on the parent, so a parent that did not change means that the leaf holds `key`. A root leaf has no parent, and a root split changes its kind.
                    if guard[KIND] == L::LEAF as u8 && parent.as_ref().is_none_or(|p| p.is_valid())
                    {
                        return Ok((guard, fence));
                    }
                    continue 'restart;
                }
                let step = (kind == Some(L::INNER as u8))
                    .then(|| {
                        inner::level_below(&Read(&node), above).zip(inner::step(&Read(&node), key))
                    })
                    .flatten();
                let Some((level, step)) = step else {
                    if node.is_valid() {
                        return Err(corrupt(
                            node.page(),
                            "has a bad kind, a bad level or bad inner bytes",
                        ));
                    }
                    continue 'restart;
                };
                let child = pages.optimistic(step.child);
                if !node.is_valid() {
                    continue 'restart;
                }
                fence = step.fence.or(fence);
                above = level;
                parent = Some(node);
                node = child?;
            }
        }
    }

    /// Calls `f` on the leaf for `key` with a shared latch.
    pub fn read<P: PageAccess, T>(
        &self,
        pages: &P,
        key: &[u8],
        f: impl FnOnce(&Page) -> Result<T>,
    ) -> Result<T> {
        let (leaf, _) = self.find(pages, key, P::shared)?;
        f(&leaf)
    }

    /// Calls `f` on the leaf for `key` with an exclusive latch. When `f` gives [`Write::Full`], the tree splits the leaf and calls `f` again on the leaf that then holds `key`.
    pub fn write<P: PageAccess, T>(
        &self,
        pages: &P,
        key: &[u8],
        mut f: impl FnMut(&mut Page) -> Result<Write<T>>,
    ) -> Result<T> {
        if key.len() > MAX_KEY {
            return Err(Error::internal(format!(
                "a tree key of {} bytes is longer than {MAX_KEY}",
                key.len()
            )));
        }
        for _ in 0..=MAX_SPLITS {
            let (mut leaf, _) = self.find(pages, key, P::exclusive)?;
            if let Write::Done(t) = f(&mut leaf)? {
                return Ok(t);
            }
            drop(leaf);

            // Go down again from the root with exclusive latches. `path` holds the latched nodes from the highest node that a split can change.
            let mut path: Vec<(PageId, P::Exclusive<'_>)> =
                vec![(self.root, pages.exclusive(self.root)?)];
            let mut above = u16::MAX;
            loop {
                let (id, top) =
                    path.last().ok_or_else(|| Error::internal("the tree path is empty"))?;
                let kind = top[KIND];
                if kind == L::LEAF as u8 {
                    break;
                }
                if kind != L::INNER as u8 {
                    return Err(corrupt(*id, "has a bad kind"));
                }
                above = inner::level_below(&**top, above)
                    .ok_or_else(|| corrupt(*id, "has a bad level"))?;
                let step =
                    inner::step(&**top, key).ok_or_else(|| corrupt(*id, "has bad inner bytes"))?;
                let child = pages.exclusive(step.child)?;
                if child[KIND] == L::INNER as u8 && inner::fits(&child, MAX_KEY) {
                    path.clear();
                }
                path.push((step.child, child));
            }
            let (_, leaf) =
                path.last_mut().ok_or_else(|| Error::internal("the tree path is empty"))?;
            if let Write::Done(t) = f(leaf)? {
                return Ok(t);
            }
            self.split(pages, &mut path, key)?;
        }
        Err(Error::internal(format!("a tree write did not fit after {MAX_SPLITS} splits")))
    }

    /// Splits the leaf at the end of `path` and each full inner node above it.
    fn split<'a, P: PageAccess>(
        &self,
        pages: &'a P,
        path: &mut [(PageId, P::Exclusive<'a>)],
        key: &[u8],
    ) -> Result<()> {
        let Some(((leaf_id, leaf), above)) = path.split_last_mut() else {
            return Err(Error::internal("the tree path is empty"));
        };
        let check = |sep: Vec<u8>| match sep.len() <= MAX_KEY {
            true => Ok(sep),
            false => Err(Error::internal(format!(
                "a leaf split gave a separator of {} bytes",
                sep.len()
            ))),
        };
        if *leaf_id == self.root {
            let (a, mut left) = pages.allocate()?;
            left.copy_from_slice(&leaf[..]);
            left[16..24].copy_from_slice(&a.0.to_le_bytes());
            let (b, mut right) = pages.allocate()?;
            L::init(&mut right, b);
            let sep = check(L::split(&mut left, &mut right, key)?)?;
            return inner::build(leaf, self.root, L::INNER, 1, a, &[(sep, b)]);
        }
        let (b, mut right) = pages.allocate()?;
        L::init(&mut right, b);
        let mut pending: Entry = (check(L::split(leaf, &mut right, key)?)?, b);
        drop(right);

        for (id, node) in above.iter_mut().rev() {
            if inner::fits(node, pending.0.len()) {
                return inner::insert(node, &pending.0, pending.1);
            }
            let level = inner::level(node);
            let mut all = inner::entries(node)?;
            let at = all.partition_point(|(k, _)| *k < pending.0);
            all.insert(at, pending);
            let (left, (up, right_first), right) = inner::halves(all);
            let (b, mut page) = pages.allocate()?;
            inner::build(&mut page, b, L::INNER, level, right_first, &right)?;
            if *id == self.root {
                let (a, mut page) = pages.allocate()?;
                inner::build(&mut page, a, L::INNER, level, inner::leftmost(node), &left)?;
                return inner::build(node, self.root, L::INNER, level + 1, a, &[(up, b)]);
            }
            let first = inner::leftmost(node);
            inner::build(node, *id, L::INNER, level, first, &left)?;
            pending = (up, b);
        }
        Err(Error::internal("a tree split reached a node that it does not hold"))
    }

    /// Calls `f` on a copy of each leaf from the leaf for `from`, in key order. The scan copies the leaf under a shared latch and releases the latch before it calls `f`, so `f` can read and change the tree. The second argument of `f` is the lowest key of the scan in that leaf: `from` for the first leaf and the lower bound of the leaf after it. The scan stops when `f` gives false.
    ///
    /// A scan sees each key that was in the tree for the whole scan. It can see a key that a writer adds during the scan, or not.
    pub fn scan<P: PageAccess>(
        &self,
        pages: &P,
        from: &[u8],
        mut f: impl FnMut(&Page, &[u8]) -> Result<bool>,
    ) -> Result<()> {
        let mut lower = from.to_vec();
        let mut copy: Box<Page> = Box::new([0; rupg_file::PAGE_SIZE]);
        loop {
            let (leaf, fence) = self.find(pages, &lower, P::shared)?;
            copy.copy_from_slice(&leaf[..]);
            drop(leaf);
            if !f(&copy, &lower)? {
                return Ok(());
            }
            match fence {
                Some(fence) => lower = fence,
                None => return Ok(()),
            }
        }
    }

    /// Checks the whole tree: the kinds, the page numbers, the levels, the order of the separators, and that each leaf holds only keys between its bounds. Bad bytes give SQLSTATE `XX001`. No writer may change the tree during the check.
    pub fn check<P: PageAccess>(&self, pages: &P) -> Result<Shape> {
        self.check_with(pages, &mut |_, _| Ok(()))
    }

    /// Does [`Tree::check`] and calls `visit` once on each page of the tree after the check of the page. An error from `visit` stops the check.
    pub fn check_with<P: PageAccess>(
        &self,
        pages: &P,
        visit: &mut dyn FnMut(PageId, &Page) -> Result<()>,
    ) -> Result<Shape> {
        let mut walk = Walk { shape: Shape::default(), visit };
        let level = self.check_node(pages, self.root, None, None, None, &mut walk)?;
        walk.shape.depth = level + 1;
        Ok(walk.shape)
    }

    fn check_node<P: PageAccess>(
        &self,
        pages: &P,
        id: PageId,
        level: Option<u16>,
        lower: Option<&[u8]>,
        upper: Option<&[u8]>,
        walk: &mut Walk<'_>,
    ) -> Result<u16> {
        let page: Box<Page> = Box::new(*pages.shared(id)?);
        let header = PageHeader::read(&page)?;
        if header.page != id {
            return Err(corrupt(id, &format!("holds the page number {}", header.page)));
        }
        let in_bounds = |k: &[u8], strict: bool| {
            let low = match lower {
                Some(l) if strict => k > l,
                Some(l) => k >= l,
                None => true,
            };
            low && upper.is_none_or(|u| k < u)
        };
        if header.kind == L::LEAF {
            if level.is_some_and(|l| l != 0) {
                return Err(corrupt(id, "is a leaf above level 0"));
            }
            let bad = L::bounds(&page)?.is_some_and(|(low, high)| {
                low > high || !in_bounds(&low, false) || !in_bounds(&high, false)
            });
            if bad {
                return Err(corrupt(id, "holds a key outside its bounds"));
            }
            walk.shape.leaves += 1;
            (walk.visit)(id, &page)?;
            return Ok(0);
        }
        if header.kind != L::INNER {
            return Err(corrupt(id, &format!("has the kind {}", header.kind)));
        }
        let own = inner::level(&page);
        if own == 0 || level.is_some_and(|l| l != own) {
            return Err(corrupt(id, &format!("has the level {own}")));
        }
        let entries = inner::entries(&page)?;
        if entries.iter().any(|(k, _)| !in_bounds(k, true)) {
            return Err(corrupt(id, "holds a separator outside its bounds"));
        }
        walk.shape.inner += 1;
        (walk.visit)(id, &page)?;
        let mut low = lower;
        let mut child = inner::leftmost(&page);
        for (key, next) in &entries {
            self.check_node(pages, child, Some(own - 1), low, Some(key.as_slice()), walk)?;
            low = Some(key.as_slice());
            child = *next;
        }
        self.check_node(pages, child, Some(own - 1), low, upper, walk)?;
        Ok(own)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use rupg_buffer::{BufferPool, MemStore, PoolConfig, Store};
    use rupg_platform::os::OsTasks;
    use rupg_platform::sim::SimRng;
    use rupg_platform::{MemoryPool, TaskHandle, Tasks};

    use super::*;
    use crate::mem::MemPages;

    /// A leaf of `u64` keys and `u64` values. The tree key is the key in big endian, padded to 128 bytes, so that a few thousand keys give a tree of depth 3.
    struct TestLeaf;

    const CAP: usize = 16;
    const COUNT: usize = 40;
    const ROWS: usize = 64;

    fn key(n: u64) -> Vec<u8> {
        let mut k = vec![0; 128];
        k[..8].copy_from_slice(&n.to_be_bytes());
        k
    }

    fn rows(page: &Page) -> Vec<(u64, u64)> {
        let n = usize::from(u16::from_le_bytes([page[COUNT], page[COUNT + 1]])).min(CAP);
        (0..n)
            .map(|i| {
                let at = ROWS + 16 * i;
                let k = u64::from_le_bytes(page[at..at + 8].try_into().unwrap());
                (k, u64::from_le_bytes(page[at + 8..at + 16].try_into().unwrap()))
            })
            .collect()
    }

    fn set_rows(page: &mut Page, rows: &[(u64, u64)]) {
        page[COUNT..COUNT + 2].copy_from_slice(&(rows.len() as u16).to_le_bytes());
        page[ROWS..ROWS + 16 * CAP].fill(0);
        for (i, (k, v)) in rows.iter().enumerate() {
            let at = ROWS + 16 * i;
            page[at..at + 8].copy_from_slice(&k.to_le_bytes());
            page[at + 8..at + 16].copy_from_slice(&v.to_le_bytes());
        }
    }

    impl Leaf for TestLeaf {
        const INNER: PageKind = PageKind::HotInner;
        const LEAF: PageKind = PageKind::HotLeaf;

        fn init(page: &mut Page, id: PageId) {
            page.fill(0);
            PageHeader::new(id, PageKind::HotLeaf).write(page);
        }

        fn split(left: &mut Page, right: &mut Page, _key: &[u8]) -> Result<Vec<u8>> {
            let all = rows(left);
            if all.len() < 2 {
                return Err(Error::internal("a leaf with one row cannot split"));
            }
            let (low, high) = all.split_at(all.len() / 2);
            set_rows(left, low);
            set_rows(right, high);
            Ok(key(high[0].0))
        }

        fn bounds(page: &Page) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
            let all = rows(page);
            Ok(all.first().zip(all.last()).map(|(a, b)| (key(a.0), key(b.0))))
        }
    }

    type T = Tree<TestLeaf>;

    fn put<P: PageAccess>(tree: &T, pages: &P, k: u64, v: u64) -> Result<Option<u64>> {
        tree.write(pages, &key(k), |page| {
            let mut all = rows(page);
            match all.binary_search_by_key(&k, |r| r.0) {
                Ok(i) => {
                    let old = all[i].1;
                    all[i].1 = v;
                    set_rows(page, &all);
                    Ok(Write::Done(Some(old)))
                }
                Err(_) if all.len() == CAP => Ok(Write::Full),
                Err(i) => {
                    all.insert(i, (k, v));
                    set_rows(page, &all);
                    Ok(Write::Done(None))
                }
            }
        })
    }

    fn get<P: PageAccess>(tree: &T, pages: &P, k: u64) -> Option<u64> {
        let all = tree.read(pages, &key(k), |page| Ok(rows(page))).unwrap();
        all.iter().find(|r| r.0 == k).map(|r| r.1)
    }

    fn scan<P: PageAccess>(tree: &T, pages: &P, from: u64, limit: usize) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        tree.scan(pages, &key(from), |page, lower| {
            let lower = u64::from_be_bytes(lower[..8].try_into().unwrap());
            out.extend(rows(page).into_iter().filter(|r| r.0 >= lower));
            out.truncate(limit);
            Ok(out.len() < limit)
        })
        .unwrap();
        out
    }

    /// Keys in order, in reverse order and at random. Each tree agrees with a map, and the check passes.
    fn fill<P: PageAccess>(pages: &P) {
        const N: u64 = 20_000;
        let mut rng = SimRng::new(7);
        let mut shuffled: Vec<u64> = (0..N).collect();
        for i in (1..shuffled.len()).rev() {
            shuffled.swap(i, rng.below(i as u64 + 1) as usize);
        }
        let orders: [Vec<u64>; 3] = [(0..N).collect(), (0..N).rev().collect(), shuffled];
        for order in orders {
            let tree = T::create(pages).unwrap();
            let root = tree.root();
            let mut model = BTreeMap::new();
            for &k in &order {
                assert_eq!(put(&tree, pages, k * 3, k), Ok(None));
                model.insert(k * 3, k);
            }
            assert_eq!(tree.root(), root);
            assert_eq!(put(&tree, pages, 300, 1), Ok(Some(100)));
            model.insert(300, 1);
            let shape = tree.check(pages).unwrap();
            assert_eq!(shape.depth, 3, "{shape:?}");
            assert!(shape.leaves >= N / CAP as u64, "{shape:?}");
            for k in 0..N * 3 {
                assert_eq!(get(&tree, pages, k), model.get(&k).copied());
            }
            let all = scan(&T::open(root), pages, 0, usize::MAX);
            assert_eq!(all, model.iter().map(|(&k, &v)| (k, v)).collect::<Vec<_>>());
            let some = scan(&tree, pages, 1_000, 50);
            assert_eq!(
                some,
                model.range(1_000..).take(50).map(|(&k, &v)| (k, v)).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn a_tree_in_memory() {
        fill(&MemPages::new(10_000));
    }

    #[test]
    fn a_tree_in_the_buffer_pool() {
        let store = Arc::new(MemStore::new()) as Arc<dyn Store>;
        let memory = MemoryPool::new(64 << 20);
        let pool =
            BufferPool::new(store, &memory, PoolConfig::new(512 * rupg_file::PAGE_SIZE as u64))
                .unwrap();
        fill(&*pool);
    }

    #[test]
    fn a_new_tree_is_one_leaf() {
        let pages = MemPages::new(8);
        let tree = T::create(&pages).unwrap();
        assert_eq!(tree.check(&pages).unwrap(), Shape { depth: 1, inner: 0, leaves: 1 });
        assert_eq!(get(&tree, &pages, 5), None);
        assert_eq!(scan(&tree, &pages, 0, 10), vec![]);
        assert!(tree.write(&pages, &[0; MAX_KEY + 1], |_| Ok(Write::Done(()))).is_err());
    }

    /// A write that never fits splits its leaf until the leaf format gives an error. The tree stays valid.
    #[test]
    fn a_write_that_never_fits() {
        let pages = MemPages::new(64);
        let tree = T::create(&pages).unwrap();
        for k in 0..40 {
            put(&tree, &pages, k, k).unwrap();
        }
        let e = tree.write(&pages, &key(7), |_| Ok(Write::<()>::Full)).unwrap_err();
        assert!(e.message().contains("one row"), "{e:?}");
        tree.check(&pages).unwrap();
        for k in 0..40 {
            assert_eq!(get(&tree, &pages, k), Some(k));
        }
    }

    #[test]
    fn bad_pages() {
        let pages = MemPages::new(64);
        let tree = T::create(&pages).unwrap();
        for k in 0..100 {
            put(&tree, &pages, k, k).unwrap();
        }
        let child = inner::leftmost(&pages.shared(tree.root()).unwrap());
        pages.exclusive(child).unwrap()[KIND] = PageKind::Toast as u8;
        let e = tree.read(&pages, &key(0), |_| Ok(())).unwrap_err();
        assert_eq!(e.state(), rupg_common::SqlState::DATA_CORRUPTED);
        assert_eq!(tree.check(&pages).unwrap_err().state(), rupg_common::SqlState::DATA_CORRUPTED);
        pages.exclusive(child).unwrap()[KIND] = PageKind::HotLeaf as u8;
        tree.check(&pages).unwrap();

        // A key in the wrong leaf.
        let mut page = pages.exclusive(child).unwrap();
        let mut all = rows(&page);
        all[0].0 = 1_000;
        set_rows(&mut page, &all);
        drop(page);
        assert!(tree.check(&pages).is_err());

        // A child number that points up the tree. A read and a write give an error and do not loop.
        all[0].0 = 0;
        set_rows(&mut pages.exclusive(child).unwrap(), &all);
        tree.check(&pages).unwrap();
        let root = tree.root();
        pages.exclusive(root).unwrap()[inner::LEFTMOST..][..8]
            .copy_from_slice(&root.0.to_le_bytes());
        let e = tree.read(&pages, &key(0), |_| Ok(())).unwrap_err();
        assert_eq!(e.state(), rupg_common::SqlState::DATA_CORRUPTED);
        let e = put(&tree, &pages, 0, 1).unwrap_err();
        assert_eq!(e.state(), rupg_common::SqlState::DATA_CORRUPTED);
        assert!(tree.check(&pages).is_err());

        // A root that is not a tree page.
        let other = MemPages::new(4);
        let (id, mut page) = other.allocate().unwrap();
        PageHeader::new(id, PageKind::Toast).write(&mut page);
        drop(page);
        assert!(T::open(id).read(&other, &key(1), |_| Ok(())).is_err());
    }

    /// Threads write and read together, in `rounds` new trees. Each thread writes its own keys and reads them back. A scan thread checks that the keys come in order and that a key that was in the tree before the scan is in the scan.
    fn threads<P: PageAccess + 'static>(pages: Arc<P>, rounds: u64) {
        const THREADS: u64 = 8;
        const KEYS: u64 = 3_000;
        for round in 0..rounds {
            let tree = T::create(&*pages).unwrap();
            for k in 0..200 {
                put(&tree, &*pages, k * THREADS * KEYS, 0).unwrap();
            }
            let mut handles: Vec<TaskHandle> = (0..THREADS)
                .map(|t| {
                    let pages = pages.clone();
                    let task = Box::new(move || {
                        let mut rng = SimRng::new(round * THREADS + t);
                        for i in 0..KEYS {
                            let k = rng.below(THREADS * KEYS * 200) / THREADS * THREADS + t;
                            put(&tree, &*pages, k, i).unwrap();
                            assert_eq!(get(&tree, &*pages, k), Some(i));
                        }
                    });
                    OsTasks.spawn(&format!("tree {t}"), task).unwrap()
                })
                .collect();
            let scanner = {
                let pages = pages.clone();
                Box::new(move || {
                    for _ in 0..20 {
                        let all = scan(&tree, &*pages, 0, usize::MAX);
                        assert!(all.windows(2).all(|w| w[0].0 < w[1].0));
                        for k in 0..200 {
                            assert!(
                                all.binary_search_by_key(&(k * THREADS * KEYS), |r| r.0).is_ok()
                            );
                        }
                    }
                })
            };
            handles.push(OsTasks.spawn("tree scan", scanner).unwrap());
            for h in handles {
                h.join().unwrap();
            }
            let shape = tree.check(&*pages).unwrap();
            assert!(shape.depth >= 2, "{shape:?}");
        }
    }

    #[test]
    fn threads_in_memory() {
        threads(Arc::new(MemPages::new(40_000)), 8);
    }

    #[test]
    fn threads_in_the_buffer_pool() {
        let store = Arc::new(MemStore::new()) as Arc<dyn Store>;
        let memory = MemoryPool::new(64 << 20);
        let pool =
            BufferPool::new(store, &memory, PoolConfig::new(256 * rupg_file::PAGE_SIZE as u64))
                .unwrap();
        threads(pool, 2);
    }
}
