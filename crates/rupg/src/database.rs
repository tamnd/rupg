//! The database handle of M1: open, recovery, the tables and the checkpoints.

use std::collections::BTreeMap;
use std::ops::{Bound, RangeBounds};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rupg_buffer::{BufferPool, Checkpoint, FileStore, PoolConfig, Store};
use rupg_common::{Error, Hlc, Oid, Result, RowId, ShardId, SqlState, Xid};
use rupg_log::{Log, Recovery, Ring};
use rupg_platform::os::{OsClock, OsEntropy, OsIo};
use rupg_platform::{Clock, Entropy, Io, MemoryPool, OpenMode};
use rupg_table::{ColumnDef, TableDef};
use rupg_txn::{HlcClock, Isolation, Transactions, replay};
use rupg_types::Datum;

use crate::catalog::{Contents, Entry};

/// The ring of the log at M1. The M1 log has one ring.
const RING: u32 = 0;

/// Each `Database` gets its own number, so that a table handle of one database is not used with another.
static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

/// The options of [`Database::open`].
#[derive(Clone, Debug)]
pub struct Options {
    /// The memory limit in bytes, `rupg.memory_limit`. The buffer pool takes its frames from it. The default is 256 MiB.
    pub memory_limit: u64,
    /// The size of the buffer pool in bytes, `shared_buffers`. The default is 128 MiB.
    pub shared_buffers: u64,
    /// The number of extents of 16 MiB in the log ring of a new file. The default is 4.
    pub log_extents: usize,
    /// A transaction that starts when the log holds more than this many bytes after the redo position takes a checkpoint first. The default is half the ring.
    pub checkpoint_log: Option<u64>,
    /// Make the file if it does not exist. The default is true. When it is false, a missing file gives SQLSTATE `58P01`.
    pub create: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            memory_limit: 256 << 20,
            shared_buffers: 128 << 20,
            log_extents: 4,
            checkpoint_log: None,
            create: true,
        }
    }
}

/// The platform that a database runs on. [`Platform::os`] gives the operating system. The tests give the simulation of `rupg_platform::sim`.
#[derive(Clone, Debug)]
pub struct Platform {
    /// The file system.
    pub io: Arc<dyn Io>,
    /// The clock.
    pub clock: Arc<dyn Clock>,
    /// The random source, for the file id of a new file.
    pub entropy: Arc<dyn Entropy>,
}

impl Platform {
    /// The operating system.
    pub fn os() -> Platform {
        Platform {
            io: Arc::new(OsIo),
            clock: Arc::new(OsClock::new()),
            entropy: Arc::new(OsEntropy),
        }
    }
}

/// A table of a database. Get one from [`Database::create_table`] or [`Database::table`]. The handle is cheap to clone.
#[derive(Clone, Debug)]
pub struct Table {
    pub(crate) database: u64,
    pub(crate) inner: Arc<rupg_table::Table>,
}

impl Table {
    /// The name.
    pub fn name(&self) -> &str {
        self.inner.def().name()
    }

    /// The OID.
    pub fn oid(&self) -> Oid {
        self.inner.def().oid()
    }

    /// The columns, in order.
    pub fn columns(&self) -> &[ColumnDef] {
        self.inner.def().columns()
    }
}

/// An open database file. It is `Send` and `Sync`, and a clone is cheap. Each clone uses the same file.
#[derive(Clone, Debug)]
pub struct Database {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    number: u64,
    store: Arc<FileStore>,
    txns: Arc<Transactions<BufferPool>>,
    checkpoint_log: u64,
    catalog: Mutex<Catalog>,
    /// One checkpoint at a time. A new table holds it from its creation to the checkpoint that makes it durable.
    checkpoints: Mutex<()>,
}

#[derive(Debug)]
struct Catalog {
    page: rupg_file::PageId,
    next_oid: Oid,
    /// The tables by name. A table is ready after the checkpoint that writes it to the catalog page.
    tables: BTreeMap<String, (Arc<rupg_table::Table>, bool)>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Database {
    /// Opens the database file at `path` on the operating system, and makes it if it does not exist and `options.create` is true. When the file was not closed cleanly, the open replays the log (spec/11 section 11.16).
    pub fn open(path: impl AsRef<Path>, options: Options) -> Result<Database> {
        Database::open_on(&Platform::os(), path.as_ref(), &options)
    }

    /// Opens the database file at `path` on `platform`.
    pub fn open_on(platform: &Platform, path: &Path, options: &Options) -> Result<Database> {
        let io = &platform.io;
        let exists = io.exists(path)?;
        if !exists && !options.create {
            return Err(Error::new(
                SqlState::UNDEFINED_FILE,
                format!("database file \"{}\" does not exist", path.display()),
            ));
        }
        let file = io.open(path, if exists { OpenMode::ReadWrite } else { OpenMode::CreateNew })?;
        // A file of zero bytes is a file that a crash stopped before its first write.
        let new = file.size()? == 0;
        let store = if new {
            let mut id = [0; 16];
            platform.entropy.fill(&mut id);
            let now = Hlc::new(platform.clock.hlc_wall(), 0)
                .ok_or_else(|| Error::internal("the wall clock is past the range of the HLC"))?;
            FileStore::create(file, id, now)?
        } else {
            FileStore::open(file)?
        };
        let store = Arc::new(store);
        let (_, slot) = store.slot();
        let contents = match slot.catalog.page {
            0 => Contents::new(),
            _ => Contents::read(&store, slot.catalog)?,
        };
        let page = match slot.catalog.page {
            0 => store.allocate()?,
            n => rupg_file::PageId(n),
        };

        let memory = MemoryPool::new(options.memory_limit);
        let pool =
            BufferPool::new(store.clone(), &memory, PoolConfig::new(options.shared_buffers))?;
        let mut tables = BTreeMap::new();
        for e in contents.tables {
            let name = e.def.name().to_owned();
            let table = rupg_table::Table::open(e.def, ShardId(0), e.root, e.next_local);
            tables.insert(name, (Arc::new(table), true));
        }
        let by_oid: BTreeMap<Oid, Arc<rupg_table::Table>> =
            tables.values().map(|(t, _)| (t.def().oid(), t.clone())).collect();

        // Recovery: the pages of the last checkpoint, then the safe blocks of the log.
        let rec = Recovery::scan(&store)?;
        let done = replay(&*pool, &store, &rec, &|oid| by_oid.get(&oid).cloned())?;
        pool.flush_all()?;
        let next_xid = contents.next_xid.max(done.next_xid.unwrap_or(Xid::FIRST));
        let floor = done.newest.max(slot.checkpoint);
        let catalog = Catalog { page, next_oid: contents.next_oid, tables };
        let root = catalog.contents(next_xid).write(&store, page, floor)?;
        let base = Checkpoint { timestamp: floor, catalog: root, ..Checkpoint::default() };
        let log = if store.rings().is_empty() {
            let ring = Ring::create(store.clone(), RING, 0, options.log_extents.max(1))?;
            store.checkpoint(&Checkpoint { rings: vec![ring.state()], ..base })?;
            Log::new(vec![ring])?
        } else {
            rec.finish(store.clone(), &base)?
        };
        if new {
            let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
            io.sync_dir(dir)?;
        }

        let size = log.ring(RING as u16)?.size();
        let clock = HlcClock::new(platform.clock.clone(), floor);
        let txns = Arc::new(Transactions::new(pool, clock, next_xid).with_log(Arc::new(log)));
        txns.filter_writes()?;
        let shared = Shared {
            number: NEXT_DATABASE.fetch_add(1, Ordering::Relaxed),
            store,
            txns,
            checkpoint_log: options.checkpoint_log.unwrap_or(size / 2),
            catalog: Mutex::new(catalog),
            checkpoints: Mutex::new(()),
        };
        Ok(Database { shared: Arc::new(shared) })
    }

    /// Makes a table with `columns` and writes it to the catalog with a checkpoint. A name that a table has gives SQLSTATE `42P07`. The errors of the columns are those of [`TableDef::new`].
    pub fn create_table(&self, name: &str, columns: Vec<ColumnDef>) -> Result<Table> {
        let s = &*self.shared;
        let _one = lock(&s.checkpoints);
        let table = {
            let mut catalog = lock(&s.catalog);
            if catalog.tables.contains_key(name) {
                return Err(Error::new(
                    SqlState::DUPLICATE_TABLE,
                    format!("relation \"{name}\" already exists"),
                ));
            }
            let oid = catalog.next_oid;
            let def = TableDef::new(oid, name, columns)?;
            let table = Arc::new(rupg_table::Table::create(&**s.txns.pages(), def, ShardId(0))?);
            catalog.next_oid = Oid(oid.0 + 1);
            catalog.tables.insert(name.to_owned(), (table.clone(), false));
            table
        };
        if let Err(e) = self.checkpoint_locked(false) {
            // The table can be in the file or not. The next open finds out from the catalog page.
            lock(&s.catalog).tables.remove(name);
            return Err(e);
        }
        if let Some(entry) = lock(&s.catalog).tables.get_mut(name) {
            entry.1 = true;
        }
        Ok(Table { database: s.number, inner: table })
    }

    /// The table `name`. A name with no table gives SQLSTATE `42P01`.
    pub fn table(&self, name: &str) -> Result<Table> {
        match lock(&self.shared.catalog).tables.get(name) {
            Some((t, true)) => Ok(Table { database: self.shared.number, inner: t.clone() }),
            _ => Err(Error::new(
                SqlState::UNDEFINED_TABLE,
                format!("relation \"{name}\" does not exist"),
            )),
        }
    }

    /// The names of the tables, in order.
    pub fn table_names(&self) -> Vec<String> {
        let catalog = lock(&self.shared.catalog);
        catalog.tables.iter().filter(|(_, (_, ready))| *ready).map(|(n, _)| n.clone()).collect()
    }

    /// Starts a transaction at `READ COMMITTED`, the default of PostgreSQL.
    pub fn transaction(&self) -> Result<Transaction> {
        self.transaction_with(Isolation::ReadCommitted)
    }

    /// Starts a transaction at `isolation`. When the log holds more than [`Options::checkpoint_log`] bytes after the redo position, it takes a checkpoint first.
    pub fn transaction_with(&self, isolation: Isolation) -> Result<Transaction> {
        if self.log_bytes()? > self.shared.checkpoint_log {
            // When another thread holds the lock, it writes a checkpoint now.
            if let Ok(_one) = self.shared.checkpoints.try_lock() {
                self.checkpoint_locked(false)?;
            }
        }
        Ok(Transaction { database: self.shared.number, inner: self.shared.txns.begin(isolation) })
    }

    /// The bytes of the log after the redo position, which a recovery would read.
    fn log_bytes(&self) -> Result<u64> {
        let Some(log) = self.shared.txns.log() else { return Ok(0) };
        let ring = log.ring(RING as u16)?;
        Ok(ring.end().saturating_sub(ring.state().redo))
    }

    /// Writes a checkpoint. After it, recovery reads only the log after it.
    pub fn checkpoint(&self) -> Result<()> {
        self.write_checkpoint(false)
    }

    /// Writes a checkpoint that marks a clean close. The other clones of the handle can still be used, and a later commit makes the file not clean again.
    pub fn close(self) -> Result<()> {
        self.write_checkpoint(true)
    }

    fn write_checkpoint(&self, clean: bool) -> Result<()> {
        let _one = lock(&self.shared.checkpoints);
        self.checkpoint_locked(clean)
    }

    /// Writes a checkpoint. The caller holds the checkpoint lock.
    fn checkpoint_locked(&self, clean: bool) -> Result<()> {
        let s = &*self.shared;
        s.txns.checkpoint(&s.store, |w| {
            let catalog = lock(&s.catalog);
            let root = catalog.contents(s.txns.next_xid()).write(&s.store, catalog.page, w)?;
            Ok(Checkpoint { catalog: root, clean_shutdown: clean, ..Checkpoint::default() })
        })?;
        Ok(())
    }
}

impl Catalog {
    fn contents(&self, next_xid: Xid) -> Contents {
        let tables = self
            .tables
            .values()
            .map(|(t, _)| Entry {
                def: t.def().clone(),
                root: t.root(),
                next_local: t.next_local(),
            })
            .collect();
        Contents { next_xid, next_oid: self.next_oid, tables }
    }
}

/// A transaction. It rolls back when it is dropped without [`Transaction::commit`]. Each change is one command, so a later read of the same transaction sees it.
#[derive(Debug)]
pub struct Transaction {
    database: u64,
    inner: rupg_txn::Transaction<BufferPool>,
}

impl Transaction {
    fn check(&self, table: &Table) -> Result<()> {
        if table.database == self.database {
            return Ok(());
        }
        Err(Error::new(
            SqlState::UNDEFINED_TABLE,
            format!("relation \"{}\" is not a table of this database", table.name()),
        ))
    }

    /// Adds a row and gives its row id. The values are one for each column, in order, with the errors of PostgreSQL for an `INSERT`.
    pub fn insert(&mut self, table: &Table, values: &[Datum]) -> Result<RowId> {
        self.check(table)?;
        let id = self.inner.insert(&table.inner, values)?;
        self.inner.next_command()?;
        Ok(id)
    }

    /// Sets the values of the row `id`. Gives false if the transaction sees no row `id`.
    pub fn update(&mut self, table: &Table, id: RowId, values: &[Datum]) -> Result<bool> {
        self.check(table)?;
        let found = self.inner.update(&table.inner, id, values)?;
        self.inner.next_command()?;
        Ok(found)
    }

    /// Deletes the row `id`. Gives false if the transaction sees no row `id`.
    pub fn delete(&mut self, table: &Table, id: RowId) -> Result<bool> {
        self.check(table)?;
        let found = self.inner.delete(&table.inner, id)?;
        self.inner.next_command()?;
        Ok(found)
    }

    /// The values of the row `id`, or `None` if the transaction sees no row `id`.
    pub fn get(&self, table: &Table, id: RowId) -> Result<Option<Vec<Datum>>> {
        self.check(table)?;
        self.inner.get(&table.inner, id)
    }

    /// The rows with an id in `range` that the transaction sees, in row id order.
    pub fn scan(
        &self,
        table: &Table,
        range: impl RangeBounds<RowId>,
    ) -> Result<Vec<(RowId, Vec<Datum>)>> {
        let mut out = Vec::new();
        self.scan_with(table, range, |id, values| {
            out.push((id, values));
            Ok(true)
        })?;
        Ok(out)
    }

    /// Calls `f` with each row with an id in `range` that the transaction sees, in row id order, until `f` gives false.
    pub fn scan_with(
        &self,
        table: &Table,
        range: impl RangeBounds<RowId>,
        mut f: impl FnMut(RowId, Vec<Datum>) -> Result<bool>,
    ) -> Result<()> {
        self.check(table)?;
        let from = match range.start_bound() {
            Bound::Included(&id) => id,
            Bound::Excluded(&id) if id.bits() == u64::MAX => return Ok(()),
            Bound::Excluded(&id) => RowId::from_bits(id.bits() + 1),
            Bound::Unbounded => RowId::from_bits(0),
        };
        self.inner.scan_from(&table.inner, from, |id, values| {
            if !range.contains(&id) {
                return Ok(false);
            }
            f(id, values)
        })
    }

    /// Commits. When it returns, the commit is durable in the log.
    pub fn commit(self) -> Result<()> {
        self.inner.commit().map(|_| ())
    }

    /// Rolls back.
    pub fn rollback(self) -> Result<()> {
        self.inner.rollback()
    }
}

#[cfg(test)]
mod tests;
