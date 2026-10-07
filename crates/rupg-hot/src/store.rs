//! The hot store of one table and shard: a B+ tree of PAX leaves keyed by row id (spec/10 section 10.3).

use rupg_common::{Error, Result, RowId, SqlState};
use rupg_file::{PAGE_SIZE, PageAccess, PageHeader, PageId, PageKind};
use rupg_tree::{Shape, Tree, Write};

use crate::pax::{HotLeaf, Pax, View, id_of, key, replace_in_place};
use crate::row::{Row, VersionHeader};
use crate::schema::Schema;

/// The hot store of one table and shard.
///
/// The store keeps rows and their version headers. It does not know snapshots: `rupg-txn` reads the version header and the undo records and decides which version a reader sees. All the rows of the store have the schema of the table. A schema change is an M2 feature.
#[derive(Clone, Copy, Debug)]
pub struct HotStore {
    tree: Tree<HotLeaf>,
}

impl HotStore {
    /// Makes an empty store in `pages`.
    pub fn create<P: PageAccess>(pages: &P) -> Result<HotStore> {
        Ok(HotStore { tree: Tree::create(pages)? })
    }

    /// The store with the root page `root`.
    pub fn open(root: PageId) -> HotStore {
        HotStore { tree: Tree::open(root) }
    }

    /// The root page. The catalog records it. It does not change for the life of the store.
    pub fn root(&self) -> PageId {
        self.tree.root()
    }

    /// The row with the id `id`, or `None`.
    pub fn get<P: PageAccess>(&self, pages: &P, id: RowId) -> Result<Option<Row>> {
        self.tree.read(pages, &key(id), |page| {
            let v = View::new(page)?;
            v.find(id.bits()).ok().map(|i| v.row(i)).transpose()
        })
    }

    /// Adds `row`. A row with the same id gives SQLSTATE `XX000`, because row ids are unique. A row that does not fit in an empty leaf gives `54000`, as in PostgreSQL. TOAST comes at M2.
    pub fn insert<P: PageAccess>(&self, pages: &P, schema: &Schema, row: &Row) -> Result<()> {
        fits_alone(schema, row)?;
        self.tree.write(pages, &key(row.id), |page| {
            let mut pax = Pax::for_write(page, schema)?;
            let Err(i) = pax.find(row.id) else {
                return Err(Error::internal(format!(
                    "the hot store has the row {} already",
                    row.id
                )));
            };
            pax.insert(i, row);
            if !pax.fits() {
                return Ok(Write::Full);
            }
            pax.encode(page, PageHeader::read(page)?)?;
            Ok(Write::Done(()))
        })
    }

    /// Writes `row` in place of the row with the same id. The result is false if the store has no such row.
    pub fn replace<P: PageAccess>(&self, pages: &P, schema: &Schema, row: &Row) -> Result<bool> {
        fits_alone(schema, row)?;
        self.tree.write(pages, &key(row.id), |page| {
            let Ok(i) = View::new(page)?.find(row.id.bits()) else {
                return Ok(Write::Done(false));
            };
            // An update that keeps the length of each value writes the leaf in place. Recovery replays such updates for each record.
            if replace_in_place(page, schema, i, row)? {
                return Ok(Write::Done(true));
            }
            let mut pax = Pax::for_write(page, schema)?;
            pax.remove(i);
            pax.insert(i, row);
            if !pax.fits() {
                return Ok(Write::Full);
            }
            pax.encode(page, PageHeader::read(page)?)?;
            Ok(Write::Done(true))
        })
    }

    /// Removes the row with the id `id` and gives it back. The store does not merge leaves at M1, so a leaf can become empty.
    pub fn remove<P: PageAccess>(&self, pages: &P, id: RowId) -> Result<Option<Row>> {
        self.tree.write(pages, &key(id), |page| {
            let mut pax = Pax::decode(page)?;
            let Ok(i) = pax.find(id) else {
                return Ok(Write::Done(None));
            };
            let row = pax.remove(i);
            pax.encode(page, PageHeader::read(page)?)?;
            Ok(Write::Done(Some(row)))
        })
    }

    /// Writes the version header of the row `id` in place. The result is false if the store has no such row.
    pub fn set_header<P: PageAccess>(
        &self,
        pages: &P,
        id: RowId,
        header: VersionHeader,
    ) -> Result<bool> {
        let bytes = header.encode()?;
        self.tree.write(pages, &key(id), |page| {
            let v = View::new(page)?;
            let Ok(i) = v.find(id.bits()) else {
                return Ok(Write::Done(false));
            };
            let at = v.header_at(i);
            page[at..at + VersionHeader::SIZE].copy_from_slice(&bytes);
            Ok(Write::Done(true))
        })
    }

    /// Writes the `xmax` of the row `id`. The write is in place when the leaf has an `xmax` minipage. Otherwise the leaf gets one, and that can split the leaf. The result is false if the store has no such row.
    pub fn set_xmax<P: PageAccess>(&self, pages: &P, id: RowId, xmax: u32) -> Result<bool> {
        self.tree.write(pages, &key(id), |page| {
            let v = View::new(page)?;
            let Ok(i) = v.find(id.bits()) else {
                return Ok(Write::Done(false));
            };
            if let Some(at) = v.xmax_at(i) {
                page[at..at + 4].copy_from_slice(&xmax.to_le_bytes());
                return Ok(Write::Done(true));
            }
            if xmax == 0 {
                return Ok(Write::Done(true));
            }
            let mut pax = Pax::decode(page)?;
            pax.set_xmax(i, xmax);
            if !pax.fits() {
                return match pax.len() {
                    1 => Err(too_big(pax.size())),
                    _ => Ok(Write::Full),
                };
            }
            pax.encode(page, PageHeader::read(page)?)?;
            Ok(Write::Done(true))
        })
    }

    /// Calls `f` on each row from the row `from`, in row id order, until `f` gives false. The scan copies each leaf and calls `f` with no latch, so `f` can change the store. A row that was in the store for the whole scan comes once. A row that a writer adds or changes during the scan can come in its old form or its new form.
    pub fn scan<P: PageAccess>(
        &self,
        pages: &P,
        from: RowId,
        mut f: impl FnMut(Row) -> Result<bool>,
    ) -> Result<()> {
        self.tree.scan(pages, &key(from), |page, lower| {
            let lower = id_of(lower)
                .ok_or_else(|| Error::corrupted("a hot store separator is not 8 bytes"))?;
            let v = View::new(page)?;
            let (Ok(start) | Err(start)) = v.find(lower);
            for i in start..v.len() {
                if !f(v.row(i)?)? {
                    return Ok(false);
                }
            }
            Ok(true)
        })
    }

    /// Checks each page of the store. Bad bytes give SQLSTATE `XX001`. No writer may change the store during the check.
    pub fn check<P: PageAccess>(&self, pages: &P) -> Result<Shape> {
        self.tree.check(pages)
    }

    /// Does [`HotStore::check`], and calls `page` on each page of the tree and `row` on each row of each leaf, in row id order. An error from `page` or `row` stops the check.
    pub fn check_with<P: PageAccess>(
        &self,
        pages: &P,
        page: &mut dyn FnMut(PageId) -> Result<()>,
        row: &mut dyn FnMut(Row) -> Result<()>,
    ) -> Result<Shape> {
        self.tree.check_with(pages, &mut |id, p| {
            page(id)?;
            if PageHeader::read(p)?.kind == PageKind::HotLeaf {
                let v = View::new(p)?;
                for i in 0..v.len() {
                    row(v.row(i)?)?;
                }
            }
            Ok(())
        })
    }
}

fn too_big(size: usize) -> Error {
    Error::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("row is too big: size {size}, maximum size {PAGE_SIZE}"),
    )
}

/// Checks `row` against `schema`, and checks that a leaf with only `row` fits in a page.
fn fits_alone(schema: &Schema, row: &Row) -> Result<()> {
    schema.check(row)?;
    let mut alone = Pax::empty(schema);
    alone.insert(0, row);
    match alone.fits() {
        true => Ok(()),
        false => Err(too_big(alone.size())),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use rupg_buffer::{BufferPool, MemStore, PoolConfig, Store};
    use rupg_common::ShardId;
    use rupg_platform::os::OsTasks;
    use rupg_platform::sim::SimRng;
    use rupg_platform::{MemoryPool, TaskHandle, Tasks};
    use rupg_tree::MemPages;

    use super::*;
    use crate::pax::tests::{random_row, schema};
    use crate::rowid::RowIds;
    use crate::schema::{Column, Width};

    fn rows<P: PageAccess>(store: &HotStore, pages: &P, from: u64) -> Vec<Row> {
        let mut out = Vec::new();
        store
            .scan(pages, RowId::from_bits(from), |row| {
                out.push(row);
                Ok(true)
            })
            .unwrap();
        out
    }

    /// Random changes to the store and to a map. The two agree after each step.
    #[test]
    fn agrees_with_a_map() {
        let schema = schema();
        let pages = MemPages::new(10_000);
        let store = HotStore::create(&pages).unwrap();
        let mut map: BTreeMap<u64, Row> = BTreeMap::new();
        let mut rng = SimRng::new(11);
        for step in 0..30_000 {
            let id = rng.below(5_000);
            let rid = RowId::from_bits(id);
            match rng.below(10) {
                0..=3 => {
                    let row = random_row(&mut rng, id);
                    let done = store.insert(&pages, &schema, &row);
                    assert_eq!(done.is_ok(), !map.contains_key(&id), "step {step}");
                    map.entry(id).or_insert(row);
                }
                4 | 5 => {
                    let row = random_row(&mut rng, id);
                    let done = store.replace(&pages, &schema, &row).unwrap();
                    assert_eq!(done, map.contains_key(&id));
                    if done {
                        map.insert(id, row);
                    }
                }
                6 => assert_eq!(store.remove(&pages, rid).unwrap(), map.remove(&id)),
                7 => {
                    let header = VersionHeader::owned_by(step, rng.below(1 << 48), 3);
                    assert_eq!(
                        store.set_header(&pages, rid, header).unwrap(),
                        map.contains_key(&id)
                    );
                    if let Some(row) = map.get_mut(&id) {
                        row.header = header;
                    }
                }
                8 => {
                    let xmax = rng.below(3) as u32;
                    assert_eq!(store.set_xmax(&pages, rid, xmax).unwrap(), map.contains_key(&id));
                    if let Some(row) = map.get_mut(&id) {
                        row.xmax = xmax;
                    }
                }
                _ => {
                    let want: Vec<Row> = map.range(id..).take(20).map(|(_, r)| r.clone()).collect();
                    let mut got = rows(&store, &pages, id);
                    got.truncate(20);
                    assert_eq!(got, want, "step {step}");
                }
            }
            assert_eq!(store.get(&pages, rid).unwrap().as_ref(), map.get(&id), "step {step}");
        }
        assert_eq!(rows(&store, &pages, 0), map.values().cloned().collect::<Vec<_>>());
        let shape = store.check(&pages).unwrap();
        assert!(shape.depth >= 2, "{shape:?}");
    }

    /// Rows in row id order fill the leaves, because a split at the end keeps the full leaf.
    #[test]
    fn appends_fill_the_leaves() {
        let schema =
            Schema::new(1, vec![Column { width: Width::Fixed(8), nullable: false }]).unwrap();
        let pages = MemPages::new(1_000);
        let store = HotStore::create(&pages).unwrap();
        let ids = RowIds::new(ShardId(0), 0);
        let mut block = ids.take().unwrap();
        for n in 0..100_000u64 {
            let id = block.next().unwrap_or_else(|| {
                block = ids.take().unwrap();
                block.next().unwrap()
            });
            let row = Row {
                id,
                header: VersionHeader::default(),
                xmin: 1,
                xmax: 0,
                values: vec![Some(n.to_le_bytes().to_vec())],
            };
            store.insert(&pages, &schema, &row).unwrap();
        }
        let shape = store.check(&pages).unwrap();
        // 36 bytes for each row: (16384 - 64 - 4) / 36 = 453 rows for each leaf.
        assert_eq!(shape.leaves, 100_000u64.div_ceil(453), "{shape:?}");
    }

    /// A leaf gets its xmax minipage at the first xmax that is not 0.
    #[test]
    fn xmax_minipage() {
        let schema = schema();
        let pages = MemPages::new(100);
        let store = HotStore::create(&pages).unwrap();
        let mut rng = SimRng::new(5);
        for id in 0..3 {
            let mut row = random_row(&mut rng, id);
            row.xmax = 0;
            store.insert(&pages, &schema, &row).unwrap();
        }
        let xmax = |id| store.get(&pages, RowId::from_bits(id)).unwrap().unwrap().xmax;
        assert!(store.set_xmax(&pages, RowId::from_bits(1), 0).unwrap());
        assert!(store.set_xmax(&pages, RowId::from_bits(1), 1).unwrap());
        assert_eq!((xmax(0), xmax(1), xmax(2)), (0, 1, 0));
        assert!(store.set_xmax(&pages, RowId::from_bits(2), 7).unwrap());
        assert!(store.set_xmax(&pages, RowId::from_bits(1), 0).unwrap());
        assert_eq!((xmax(0), xmax(1), xmax(2)), (0, 0, 7));
        assert!(!store.set_xmax(&pages, RowId::from_bits(3), 1).unwrap());
    }

    #[test]
    fn row_size_limit() {
        let schema =
            Schema::new(1, vec![Column { width: Width::Variable, nullable: false }]).unwrap();
        let pages = MemPages::new(100);
        let store = HotStore::create(&pages).unwrap();
        // A leaf with one row: the header, 28 bytes for the row, 4 for the directory, 4 for the offsets and the value.
        let most = PAGE_SIZE - 64 - 28 - 4 - 4;
        let row = |id, len| Row {
            id: RowId::from_bits(id),
            header: VersionHeader::default(),
            xmin: 1,
            xmax: 0,
            values: vec![Some(vec![7; len])],
        };
        store.insert(&pages, &schema, &row(5, most)).unwrap();
        let err = store.insert(&pages, &schema, &row(6, most + 1)).unwrap_err();
        assert_eq!(err.state(), SqlState::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(
            err.message(),
            format!("row is too big: size {}, maximum size {PAGE_SIZE}", PAGE_SIZE + 1)
        );
        // Two big rows on each side of a big row split down to one row in each leaf.
        store.insert(&pages, &schema, &row(3, most)).unwrap();
        store.insert(&pages, &schema, &row(9, most)).unwrap();
        store.insert(&pages, &schema, &row(4, 10)).unwrap();
        assert_eq!(
            rows(&store, &pages, 0).iter().map(|r| r.id.bits()).collect::<Vec<_>>(),
            [3, 4, 5, 9]
        );
        let err = store.replace(&pages, &schema, &row(4, most + 1)).unwrap_err();
        assert_eq!(err.state(), SqlState::PROGRAM_LIMIT_EXCEEDED);
        assert!(store.replace(&pages, &schema, &row(4, most)).unwrap());
        // A leaf with an xmax minipage has 4 bytes less for the row.
        assert!(store.set_xmax(&pages, RowId::from_bits(4), 0).unwrap());
        let err = store.set_xmax(&pages, RowId::from_bits(4), 9).unwrap_err();
        assert_eq!(err.state(), SqlState::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(store.get(&pages, RowId::from_bits(4)).unwrap().unwrap().xmax, 0);
        store.check(&pages).unwrap();
    }

    /// Threads insert rows from their own row id blocks, change them and read them back, while a scan checks the order.
    fn threads<P: PageAccess + 'static>(pages: Arc<P>, rows_each: u64) {
        const THREADS: u64 = 8;
        let schema = Arc::new(schema());
        let store = HotStore::create(&*pages).unwrap();
        let ids = Arc::new(RowIds::new(ShardId(2), 0));
        let mut handles: Vec<TaskHandle> = (0..THREADS)
            .map(|t| {
                let (pages, schema, ids) = (pages.clone(), schema.clone(), ids.clone());
                let task = Box::new(move || {
                    let mut rng = SimRng::new(t);
                    let mut block = ids.take().unwrap();
                    let mut mine = Vec::new();
                    for _ in 0..rows_each {
                        let id = match block.next() {
                            Some(id) => id,
                            None => {
                                block = ids.take().unwrap();
                                block.next().unwrap()
                            }
                        };
                        let row = random_row(&mut rng, id.bits());
                        store.insert(&*pages, &schema, &row).unwrap();
                        mine.push(row);
                        let i = rng.below(mine.len() as u64) as usize;
                        let new = random_row(&mut rng, mine[i].id.bits());
                        assert!(store.replace(&*pages, &schema, &new).unwrap());
                        mine[i] = new;
                        let i = rng.below(mine.len() as u64) as usize;
                        assert_eq!(
                            store.get(&*pages, mine[i].id).unwrap().as_ref(),
                            Some(&mine[i])
                        );
                    }
                    for row in &mine {
                        assert_eq!(store.get(&*pages, row.id).unwrap().as_ref(), Some(row));
                    }
                });
                OsTasks.spawn(&format!("hot {t}"), task).unwrap()
            })
            .collect();
        let scanner = {
            let pages = pages.clone();
            Box::new(move || {
                for _ in 0..20 {
                    let all = rows(&store, &*pages, 0);
                    assert!(all.windows(2).all(|w| w[0].id < w[1].id));
                }
            })
        };
        handles.push(OsTasks.spawn("hot scan", scanner).unwrap());
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(rows(&store, &*pages, 0).len() as u64, THREADS * rows_each);
        store.check(&*pages).unwrap();
    }

    #[test]
    fn threads_in_memory() {
        threads(Arc::new(MemPages::new(20_000)), 5_000);
    }

    #[test]
    fn threads_in_the_buffer_pool() {
        let store = Arc::new(MemStore::new()) as Arc<dyn Store>;
        let memory = MemoryPool::new(64 << 20);
        let pool =
            BufferPool::new(store, &memory, PoolConfig::new(256 * PAGE_SIZE as u64)).unwrap();
        threads(pool, 2_000);
    }
}
