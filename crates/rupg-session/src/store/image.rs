//! The image of a store: the bytes of the databases, their catalogs, the roles, the sequences and the rows of the tables, which the `.rupg` file keeps from one start of the server to the next.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use rupg_catalog::{Catalog, SequenceData};
use rupg_common::bytes::Mapping;
use rupg_common::persist::{Persist, Reader, corrupt};
use rupg_common::{Error, Result, SqlState};
use rupg_platform::{Io, OpenMode};

use super::log::{self, Log};
use super::{Db, State, Store};
use crate::heap::{Heap, Imaged, Logged};

/// The first bytes of an image.
const MAGIC: &[u8; 8] = b"rupg\x00img";
/// The size of the log after which a commit makes a checkpoint, as `max_wal_size` starts one, so a restart does not read a long log.
const LOG_LIMIT: u64 = 256 << 20;

/// The version of the form of an image. An image of another version does not load.
const VERSION: u32 = 3;

/// The place of the image of a store: the file and the I/O that writes it, and the log of the commits after the image.
#[derive(Debug)]
pub(crate) struct ImageFile {
    pub(crate) io: Arc<dyn Io>,
    pub(crate) path: PathBuf,
    log: Log,
    /// The number of the first log that the last image needs, which a checkpoint removes. The lock lets one checkpoint run at a time.
    first: Mutex<u64>,
    /// The mappings of the images whose bytes the frozen columns read. A mapping goes when no column reads it.
    maps: Mutex<Vec<Weak<dyn Mapping>>>,
}

/// The catalog of a database in an image. The catalog of `initdb` is not in the image, and a catalog that two databases share is in the image once.
enum Shared {
    Initdb,
    Earlier(usize),
    New,
}

impl Store {
    /// The store with the image of `path`, which [`Store::checkpoint`] writes. When the file exists, the store gets the databases, the roles, the sequences and the rows of its image.
    ///
    /// # Errors
    ///
    /// The error of a file that does not read, and `XX001` for a file that is not an image.
    pub fn with_image(self, io: Arc<dyn Io>, path: PathBuf) -> Result<Store> {
        let mut store = self;
        let mut first = 0;
        let mut mapped = Vec::new();
        if io.exists(&path)? {
            let file = io.open(&path, OpenMode::Read)?;
            // The frozen columns keep ranges of a mapping of the image, so that a column takes memory only while a query reads it.
            if let Some(map) = file.map() {
                first = store.load_from(&mut Reader::mapped(&map))?;
                mapped.push(Arc::downgrade(&map));
            } else {
                let len =
                    usize::try_from(file.size()?).map_err(|_| corrupt("the image is too large"))?;
                let mut bytes = vec![0; len];
                file.read_at(0, &mut bytes)?;
                first = store.load(&bytes)?;
            }
        }
        let mut epoch = first;
        while let Some(records) = log::read(&*io, &path, epoch)? {
            for record in &records {
                store.redo(record)?;
            }
            epoch += 1;
        }
        if epoch > first {
            store.drop_lost_tables();
        }
        store.heap.start_logging();
        let log = Log::create(Arc::clone(&io), path.clone(), epoch)?;
        store.image =
            Some(ImageFile { io, path, log, first: Mutex::new(first), maps: Mutex::new(mapped) });
        Ok(store)
    }

    /// Gives the pages of the mapping of the image back to the operating system, at the end of a statement. The page cache keeps them, and the next statement that reads a column maps the pages that it reads again, so the memory of the process is the columns of the statements that run (spec/19 section 19.5).
    pub(crate) fn release_mapped(&self) {
        let Some(file) = &self.image else {
            return;
        };
        let maps = file.maps.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        for map in maps.iter().filter_map(Weak::upgrade) {
            map.release(0, map.bytes().len());
        }
    }

    /// The rows of the tables that a commit of a log dropped go, as the drop of a table writes no log of its rows.
    pub(crate) fn drop_lost_tables(&self) {
        let catalogs = self.catalogs();
        for database in self.heap.databases() {
            match catalogs.iter().find(|(row, _)| row.oid == database) {
                Some((_, catalog)) => {
                    self.heap
                        .drop_tables(database, |relation| catalog.relation(relation).is_some());
                }
                None => self.heap.drop_tables(database, |_| false),
            }
        }
    }

    /// `RecordTransactionCommit` for the store: the commit of the transaction `xid` in the heap, or no commit for 0. When the store has a file, the log gets the changes of the rows and the sequences that changed, and the commit waits until the log is on the disk. With `wait` the commit waits also for the records before it, as for the catalog of [`Store::log_catalog`]. A log of only the sequences does not wait, as `nextval` does not wait for the disk.
    ///
    /// With `sync` false, for `synchronous_commit = off`, the commit of the rows does not wait for the disk, and [`Store::flush_log`] writes its record later, as `XLogSetAsyncXactLSN` leaves the flush to the WAL writer. The records stay in the order of the commits, so a crash loses only the last commits.
    pub(crate) fn commit(&self, xid: u32, wait: bool, sync: bool) {
        self.log(xid, false, wait, sync);
    }

    /// Writes the records of the log that the commits added to the disk, as the WAL writer does for the commits that did not wait.
    ///
    /// # Errors
    ///
    /// The error of a write or a sync of the log.
    pub fn flush_log(&self) -> Result<()> {
        match &self.image {
            Some(file) => file.log.wait(file.log.added(), self.tasks.as_deref()),
            None => Ok(()),
        }
    }

    /// The log gets the catalogs, the roles, the tablespaces and the settings of the store, after a commit changed them. With `wait`, the call waits until the log is on the disk.
    pub(crate) fn log_catalog(&self, wait: bool) {
        self.log(0, true, wait, true);
    }

    /// The commit of `xid`, when it is not 0, with a record of the log for its changes and for the catalog with `catalog`.
    ///
    /// # Panics
    ///
    /// When the log does not go to the disk, as PostgreSQL stops with a `PANIC` there.
    fn log(&self, xid: u32, catalog: bool, wait: bool, sync: bool) {
        let Some(file) = &self.image else {
            if xid != 0 {
                self.heap.commit(xid);
            }
            return;
        };
        let mut added = None;
        let mut large = false;
        let mut add = |logged: Logged<'_>| {
            let heap = match logged {
                Logged::Changes(bytes) => Some(bytes),
                Logged::Large => {
                    large = true;
                    None
                }
                Logged::Nothing => None,
            };
            if heap.is_some() || catalog || !self.changed_sequences().is_empty() {
                let number = file.log.add(&mut |out| self.log_record(catalog, heap, out));
                added = Some((number, heap.is_some() || catalog));
            }
        };
        if xid == 0 {
            add(Logged::Nothing);
        } else {
            self.heap.commit_logged(xid, &mut add);
        }
        if large {
            let mut first = file.first.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(error) = self.write_image(file, &mut first) {
                panic!("could not write the image after a commit: {error}");
            }
            return;
        }
        let number = match added {
            // The record of a commit that does not wait stays in the memory of the log until the next write.
            Some(_) if !sync && !catalog && !wait => None,
            Some((number, durable)) if durable || wait => Some(number),
            _ if wait => Some(file.log.added()),
            _ => return,
        };
        if let Some(number) = number
            && let Err(error) = file.log.wait(number, self.tasks.as_deref())
        {
            panic!("could not write the log of a commit: {error}");
        }
        if xid != 0 && file.log.size() > LOG_LIMIT {
            // A checkpoint that fails leaves the log, and the next commit tries again.
            if let Ok(mut first) = file.first.try_lock() {
                let _ = self.write_image(file, &mut first);
            }
        }
    }

    /// The bytes of a record of the log: the rows of the sequences that changed, `nextXid` and the catalogs when `catalog` is true, and the changes `heap` of the rows.
    fn log_record(&self, catalog: bool, heap: Option<&[u8]>, out: &mut Vec<u8>) {
        let sequences: Vec<_> = self.changed_sequences().drain().collect();
        sequences.put(out);
        catalog.put(out);
        if catalog {
            // The log of a catalog is not in a commit of the heap, so the heap is not locked.
            self.heap.next_xid().put(out);
            self.put_state(out);
        }
        heap.is_some().put(out);
        out.extend_from_slice(heap.unwrap_or_default());
    }

    /// Makes again the changes of a record of the log.
    fn redo(&mut self, record: &[u8]) -> Result<()> {
        let mut input = Reader::new(record);
        let sequences: Vec<((u32, u32), SequenceData)> = Persist::take(&mut input)?;
        self.sequences
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(sequences);
        if bool::take(&mut input)? {
            self.heap.advance_xid(u32::take(&mut input)?);
            self.state = Mutex::new(take_state(&mut input)?);
        }
        if bool::take(&mut input)? {
            self.heap.redo(&mut input)?;
        }
        if !input.is_empty() {
            return Err(corrupt("a record of the log has bytes after its end"));
        }
        Ok(())
    }

    /// `CreateCheckPoint`: writes the image of the store to its file, when the store has one. The image goes to a new file, which then takes the place of the old one, so a crash leaves the old image or the new one.
    ///
    /// # Errors
    ///
    /// The error of a write to the file.
    pub fn checkpoint(&self) -> Result<()> {
        let Some(file) = &self.image else {
            return Ok(());
        };
        let mut first = file.first.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.write_image(file, &mut first)
    }

    /// The checkpoint of [`Store::checkpoint`], with the lock of the checkpoints and the number of the first log that the last image needs.
    fn write_image(&self, file: &ImageFile, first: &mut u64) -> Result<()> {
        let epoch = file.log.next(self.tasks.as_deref())?;
        let (bytes, imaged) = self.image_of(epoch);
        let fresh = file.path.with_extension("rupg-new");
        if file.io.exists(&fresh)? {
            file.io.remove(&fresh)?;
        }
        let out = file.io.open(&fresh, OpenMode::CreateNew)?;
        out.write_at(0, &bytes)?;
        out.sync()?;
        drop(out);
        file.io.rename(&fresh, &file.path)?;
        file.io.sync_dir(file.path.parent().unwrap_or(Path::new(".")))?;
        file.log.remove(*first, epoch)?;
        *first = epoch;
        drop(bytes);
        self.map_image(file, imaged);
        Ok(())
    }

    /// The frozen columns of the image that the checkpoint wrote take the place of the columns in the memory of the process, as the columns of a load of the image read a mapping of the file. A file that does not map, or does not read back, leaves the columns as they are.
    fn map_image(&self, file: &ImageFile, imaged: Imaged) {
        let Some(map) = file.io.open(&file.path, OpenMode::Read).ok().and_then(|f| f.map()) else {
            return;
        };
        let Ok((_, _, _, heap)) = parse(&mut Reader::mapped(&map)) else {
            return;
        };
        self.heap.adopt(imaged, heap);
        let mut maps = file.maps.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        maps.retain(|old| old.strong_count() > 0);
        maps.push(Arc::downgrade(&map));
        drop(maps);
        // The columns and the versions in the memory of the process went, so the allocator gives their pages back to the system.
        rupg_platform::alloc::trim();
    }

    /// The bytes of the image of the store, whose changes go on in the logs from the log `epoch` on.
    #[cfg(test)]
    pub(crate) fn image(&self, epoch: u64) -> Vec<u8> {
        self.image_of(epoch).0
    }

    /// The bytes of [`Store::image`], with the frozen columns that went to them.
    fn image_of(&self, epoch: u64) -> (Vec<u8>, Imaged) {
        let mut out = MAGIC.to_vec();
        VERSION.put(&mut out);
        epoch.put(&mut out);
        self.put_state(&mut out);
        self.sequences.lock().unwrap_or_else(std::sync::PoisonError::into_inner).put(&mut out);
        let imaged = self.heap.image(&mut out);
        (out, imaged)
    }

    /// The bytes of the databases with their catalogs, the counter of the OIDs, the roles, the tablespaces and the settings of the databases and the roles.
    fn put_state(&self, out: &mut Vec<u8>) {
        let state = self.state();
        let initdb = crate::system::catalog();
        let mut written: Vec<Arc<Catalog>> = Vec::new();
        state.databases.len().put(out);
        for db in state.databases.values() {
            db.row.put(out);
            if Arc::ptr_eq(&db.committed, &initdb) {
                out.push(0);
            } else if let Some(at) = written.iter().position(|c| Arc::ptr_eq(c, &db.committed)) {
                out.push(1);
                at.put(out);
            } else {
                out.push(2);
                db.committed.put(out);
                written.push(Arc::clone(&db.committed));
            }
        }
        state.next_oid.put(out);
        state.roles.put(out);
        state.tablespaces.put(out);
        state.settings.put(out);
    }

    /// Takes the databases, the roles, the sequences and the rows of an image, and gives the number of the first log after the image.
    pub(crate) fn load(&mut self, bytes: &[u8]) -> Result<u64> {
        self.load_from(&mut Reader::new(bytes))
    }

    /// The load of [`Store::load`], from the bytes of `input`.
    fn load_from(&mut self, input: &mut Reader<'_>) -> Result<u64> {
        let (epoch, state, sequences, mut heap) = parse(input)?;
        heap.set_tasks(self.tasks.clone());
        self.state = Mutex::new(state);
        self.sequences = Mutex::new(sequences);
        self.heap = heap;
        Ok(epoch)
    }
}

/// The number of the first log after an image, the databases, the sequences and the heap of the bytes of [`Store::image`].
#[allow(clippy::type_complexity)]
fn parse(input: &mut Reader<'_>) -> Result<(u64, State, HashMap<(u32, u32), SequenceData>, Heap)> {
    if input.bytes(MAGIC.len()).ok() != Some(MAGIC.as_slice()) {
        return Err(corrupt("the file is not an image of rupg"));
    }
    let version = u32::take(input)?;
    if version != VERSION {
        return Err(Error::new(
            SqlState::DATA_CORRUPTED,
            format!(
                "the image has the version {version}, and this server reads the version {VERSION}"
            ),
        ));
    }
    let epoch = u64::take(input)?;
    let state = take_state(input)?;
    let sequences = Persist::take(input)?;
    let heap = Heap::from_image(input)?;
    if !input.is_empty() {
        return Err(corrupt("the image has bytes after its end"));
    }
    Ok((epoch, state, sequences, heap))
}

/// The databases, the counter of the OIDs, the roles, the tablespaces and the settings of the bytes of [`Store::put_state`].
fn take_state(input: &mut Reader<'_>) -> Result<State> {
    let initdb = crate::system::catalog();
    let mut read: Vec<Arc<Catalog>> = Vec::new();
    let mut state = State::default();
    state.databases.clear();
    for _ in 0..input.count()? {
        let row: rupg_catalog::Database = Persist::take(input)?;
        let committed = match take_shared(input)? {
            Shared::Initdb => Arc::clone(&initdb),
            Shared::Earlier(at) => {
                Arc::clone(read.get(at).ok_or_else(|| corrupt("a catalog is not in the image"))?)
            }
            Shared::New => {
                let catalog = Arc::new(Catalog::take(input)?);
                read.push(Arc::clone(&catalog));
                catalog
            }
        };
        state.databases.insert(row.oid, Db { row, committed, locked: None });
    }
    state.next_oid = Persist::take(input)?;
    state.roles = Persist::take(input)?;
    state.tablespaces = Persist::take(input)?;
    state.settings = Persist::take(input)?;
    Ok(state)
}

fn take_shared(input: &mut Reader<'_>) -> Result<Shared> {
    match u8::take(input)? {
        0 => Ok(Shared::Initdb),
        1 => Ok(Shared::Earlier(Persist::take(input)?)),
        2 => Ok(Shared::New),
        _ => Err(corrupt("a catalog of the image has a bad tag")),
    }
}
