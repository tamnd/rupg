//! `rupg upgrade`: a copy of a database file to a new file in the format of this release (spec/08 section 8.15, spec/24 Q40).
//!
//! The upgrade reads the old file through [`ReadOnly`], so the old file does not change, also when the open replays its log. It writes the new file under a temporary name and gives it its name only after the last checkpoint, so a copy that stops before the end leaves no file with the new name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rupg_common::{Error, Result, RowId, SqlState};
use rupg_platform::{File, FileMode, Io, OpenMode};

use crate::{Database, Options, Platform};

/// The rows that one transaction of the copy inserts. A large table goes into the new file in many transactions, so the log of the new file does not fill.
const BATCH: u64 = 10_000;

/// The counts of an upgrade.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Upgrade {
    /// The tables.
    pub tables: u64,
    /// The rows of the tables.
    pub rows: u64,
}

/// Copies the database file `from` to a new file `to` on the operating system. See [`upgrade_on`].
pub fn upgrade(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<Upgrade> {
    upgrade_on(&Platform::os(), from.as_ref(), to.as_ref(), &Options::default())
}

/// Copies the database file `from` to a new file `to` on `platform`, in the format of this release. The new file has each table with its name, its OID and its columns, and each committed row with its row id.
///
/// The old file must be one that this release can open. The copy opens it to read, and replays its log in memory when it was not closed cleanly, so the old file does not change. The copy writes `to` with `.partial` after the name, and renames it to `to` after its last checkpoint. A file `to` that exists gives SQLSTATE `58P02`, and a missing file `from` gives `58P01`. A file with the temporary name is removed first, because only a copy that stopped makes it.
///
/// The replay keeps the pages that it changes in memory, so an old file that was not closed cleanly needs memory for the pages that its log changes.
pub fn upgrade_on(
    platform: &Platform,
    from: &Path,
    to: &Path,
    options: &Options,
) -> Result<Upgrade> {
    let io = &platform.io;
    if io.exists(to)? {
        return Err(Error::new(
            SqlState::DUPLICATE_FILE,
            format!("database file \"{}\" exists", to.display()),
        ));
    }
    let mut partial = to.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    if io.exists(&partial)? {
        io.remove(&partial)?;
    }

    let read = Platform { io: Arc::new(ReadOnly { inner: io.clone() }), ..platform.clone() };
    let old = Database::open_on(&read, from, &Options { create: false, ..options.clone() })?;
    let new = Database::open_on(platform, &partial, &Options { create: true, ..options.clone() })?;
    let mut tables: Vec<_> =
        old.table_names().iter().map(|name| old.table(name)).collect::<Result<_>>()?;
    tables.sort_by_key(crate::Table::oid);
    let mut done = Upgrade::default();
    let snapshot = old.transaction()?;
    for table in &tables {
        let copy = new.create_table(table.name(), table.columns().to_vec())?;
        if copy.oid() != table.oid() {
            return Err(Error::internal(format!(
                "table \"{}\" has OID {} in the old file and {} in the new file",
                table.name(),
                table.oid(),
                copy.oid()
            )));
        }
        // The ids that the old file gave out stay used, also when their rows are gone.
        let next = table.inner.next_local();
        if let Some(last) = next.checked_sub(1).and_then(|l| RowId::new(table.inner.shard(), l)) {
            copy.inner.saw(last);
        }
        let mut tx = new.transaction()?;
        let mut rows: u64 = 0;
        snapshot.scan_with(table, .., |id, values| {
            tx.insert_as(&copy, id, &values)?;
            rows += 1;
            if rows.is_multiple_of(BATCH) {
                std::mem::replace(&mut tx, new.transaction()?).commit()?;
            }
            Ok(true)
        })?;
        tx.commit()?;
        done.tables += 1;
        done.rows += rows;
    }
    drop(snapshot);
    drop(old);
    new.close()?;
    io.rename(&partial, to)?;
    let dir = to.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    io.sync_dir(dir)?;
    Ok(done)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A file system that opens each file to read and keeps the writes in memory. The open of a database writes its catalog and, after a crash, the pages of its log, and these writes go to memory and not to the file.
#[derive(Debug)]
struct ReadOnly {
    inner: Arc<dyn Io>,
}

impl Io for ReadOnly {
    fn open(&self, path: &Path, mode: OpenMode) -> Result<Arc<dyn File>> {
        if matches!(mode, OpenMode::Create | OpenMode::CreateNew) {
            return Err(Error::new(
                SqlState::READ_ONLY_SQL_TRANSACTION,
                format!("could not create file \"{}\": the old file is read only", path.display()),
            ));
        }
        let base = self.inner.open(path, OpenMode::Read)?;
        let size = base.size()?;
        Ok(Arc::new(Overlay {
            base,
            state: Mutex::new(Layer { size, base_size: size, blocks: BTreeMap::new() }),
        }))
    }

    fn remove(&self, path: &Path) -> Result<()> {
        Err(Error::new(
            SqlState::READ_ONLY_SQL_TRANSACTION,
            format!("could not remove file \"{}\": the old file is read only", path.display()),
        ))
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        self.inner.exists(path)
    }

    fn sync_dir(&self, _path: &Path) -> Result<()> {
        Ok(())
    }

    fn mode(&self, path: &Path) -> Result<FileMode> {
        self.inner.mode(path)
    }
}

/// The bytes of one block of the memory layer.
const BLOCK: u64 = 4096;

/// A file of [`ReadOnly`]: the bytes of the file, with the blocks that a write changed in memory.
#[derive(Debug)]
struct Overlay {
    base: Arc<dyn File>,
    state: Mutex<Layer>,
}

#[derive(Debug)]
struct Layer {
    /// The size that reads see.
    size: u64,
    /// The bytes of the file that reads can still see. A smaller size cuts it, and a larger size after that gives zeros.
    base_size: u64,
    /// The blocks that writes changed, by block number.
    blocks: BTreeMap<u64, Box<[u8]>>,
}

impl Overlay {
    /// Reads the bytes of block `n` from `start` into `out`, from memory or from the file.
    fn read_block(&self, layer: &Layer, n: u64, start: usize, out: &mut [u8]) -> Result<()> {
        if let Some(block) = layer.blocks.get(&n) {
            out.copy_from_slice(&block[start..start + out.len()]);
            return Ok(());
        }
        out.fill(0);
        let at = n * BLOCK + start as u64;
        let end = (at + out.len() as u64).min(layer.base_size);
        if at < end {
            let len = (end - at) as usize;
            self.base.read_at(at, &mut out[..len])?;
        }
        Ok(())
    }
}

impl File for Overlay {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let layer = lock(&self.state);
        if offset.saturating_add(buf.len() as u64) > layer.size {
            return Err(Error::corrupted(format!(
                "could not read {} bytes at offset {offset}: the file is shorter",
                buf.len()
            )));
        }
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let start = (at % BLOCK) as usize;
            let len = (BLOCK as usize - start).min(buf.len() - done);
            self.read_block(&layer, at / BLOCK, start, &mut buf[done..done + len])?;
            done += len;
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        let mut layer = lock(&self.state);
        let mut done = 0;
        while done < data.len() {
            let at = offset + done as u64;
            let n = at / BLOCK;
            let start = (at % BLOCK) as usize;
            let len = (BLOCK as usize - start).min(data.len() - done);
            if !layer.blocks.contains_key(&n) {
                let mut block = vec![0; BLOCK as usize].into_boxed_slice();
                self.read_block(&layer, n, 0, &mut block)?;
                layer.blocks.insert(n, block);
            }
            if let Some(block) = layer.blocks.get_mut(&n) {
                block[start..start + len].copy_from_slice(&data[done..done + len]);
            }
            done += len;
        }
        layer.size = layer.size.max(offset + data.len() as u64);
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        Ok(())
    }

    fn size(&self) -> Result<u64> {
        Ok(lock(&self.state).size)
    }

    fn set_size(&self, size: u64) -> Result<()> {
        let mut layer = lock(&self.state);
        if size < layer.size {
            layer.base_size = layer.base_size.min(size);
            let first = size.div_ceil(BLOCK);
            layer.blocks.split_off(&first);
            // The bytes of the last block after the new size read as zeros if the file grows again.
            if !size.is_multiple_of(BLOCK)
                && let Some(block) = layer.blocks.get_mut(&(size / BLOCK))
            {
                block[(size % BLOCK) as usize..].fill(0);
            }
        }
        layer.size = size;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
