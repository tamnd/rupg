//! A table of one shard at M1: the definition, the hot store and the row id counter.

use rupg_common::{Result, RowId, ShardId};
use rupg_file::{PageAccess, PageId};
use rupg_hot::{HotStore, RowIdBlock, RowIds};

use crate::def::TableDef;

/// A table of one shard. At M1 a table has only the hot store. The cold store, TOAST and the scan that joins the two stores come at M5.
#[derive(Debug)]
pub struct Table {
    def: TableDef,
    shard: ShardId,
    hot: HotStore,
    ids: RowIds,
}

impl Table {
    /// Makes an empty table in `pages`. The row ids start at local number 1, so that no row has the row id 0.
    pub fn create<P: PageAccess>(pages: &P, def: TableDef, shard: ShardId) -> Result<Table> {
        let hot = HotStore::create(pages)?;
        Ok(Table { def, shard, hot, ids: RowIds::new(shard, 1) })
    }

    /// The table with the hot store at `root`. `next_local` is the first local row id that no block holds, from [`Table::next_local`].
    pub fn open(def: TableDef, shard: ShardId, root: PageId, next_local: u64) -> Table {
        Table { def, shard, hot: HotStore::open(root), ids: RowIds::new(shard, next_local) }
    }

    /// The definition.
    pub fn def(&self) -> &TableDef {
        &self.def
    }

    /// The shard.
    pub fn shard(&self) -> ShardId {
        self.shard
    }

    /// The hot store.
    pub fn hot(&self) -> &HotStore {
        &self.hot
    }

    /// The root page of the hot store. The catalog records it.
    pub fn root(&self) -> PageId {
        self.hot.root()
    }

    /// Takes a block of row ids for one worker.
    pub fn take_ids(&self) -> Result<RowIdBlock> {
        self.ids.take()
    }

    /// The first local row id that no block holds. The catalog records it.
    pub fn next_local(&self) -> u64 {
        self.ids.next_local()
    }

    /// Moves the row id counter past `id`. Recovery calls it for each row that it finds, so that no block gives an id that a row has.
    pub fn saw(&self, id: RowId) {
        self.ids.saw(id.local());
    }
}

#[cfg(test)]
mod tests {
    use rupg_hot::{Row, VersionHeader};
    use rupg_tree::MemPages;
    use rupg_types::Datum;

    use super::*;
    use crate::def::tests::def;

    #[test]
    fn rows() {
        let pages = MemPages::new(100);
        let table = Table::create(&pages, def(), ShardId(4)).unwrap();
        let mut ids = table.take_ids().unwrap();
        let values = [Datum::Int8(9), Datum::Text("nine".into()), Datum::Null];
        let id = ids.next().unwrap();
        assert_eq!((id.shard(), id.local()), (ShardId(4), 1));
        let row = Row {
            id,
            header: VersionHeader::default(),
            xmin: 1,
            xmax: 0,
            values: table.def().encode(&values).unwrap(),
        };
        table.hot().insert(&pages, table.def().schema(), &row).unwrap();
        let back = table.hot().get(&pages, id).unwrap().unwrap();
        assert_eq!(table.def().decode(&back.values).unwrap(), values);

        let next = table.next_local();
        assert_eq!(next, 1 + 1024);
        let again = Table::open(def(), ShardId(4), table.root(), next);
        assert_eq!(again.hot().get(&pages, id).unwrap(), Some(back));
        assert_eq!(again.take_ids().unwrap().next().map(RowId::local), Some(next));
        again.saw(RowId::new(ShardId(4), 5_000).unwrap());
        assert_eq!(again.next_local(), 5_001);
        again.saw(RowId::new(ShardId(4), 10).unwrap());
        assert_eq!(again.next_local(), 5_001);
    }
}
