//! The image of a store: the bytes of the databases, their catalogs, the roles, the sequences and the rows of the tables, which the `.rupg` file keeps from one start of the server to the next.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rupg_catalog::Catalog;
use rupg_common::persist::{Persist, Reader, corrupt};
use rupg_common::{Error, Result, SqlState};
use rupg_platform::{Io, OpenMode};

use super::{Db, State, Store};
use crate::heap::Heap;

/// The first bytes of an image.
const MAGIC: &[u8; 8] = b"rupg\x00img";
/// The version of the form of an image. An image of another version does not load.
const VERSION: u32 = 1;

/// The place of the image of a store: the file and the I/O that writes it.
#[derive(Debug)]
pub(crate) struct ImageFile {
    pub(crate) io: Arc<dyn Io>,
    pub(crate) path: PathBuf,
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
        if io.exists(&path)? {
            let file = io.open(&path, OpenMode::Read)?;
            let len =
                usize::try_from(file.size()?).map_err(|_| corrupt("the image is too large"))?;
            let mut bytes = vec![0; len];
            file.read_at(0, &mut bytes)?;
            store.load(&bytes)?;
        }
        store.image = Some(ImageFile { io, path });
        Ok(store)
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
        let bytes = self.image();
        let fresh = file.path.with_extension("rupg-new");
        if file.io.exists(&fresh)? {
            file.io.remove(&fresh)?;
        }
        let out = file.io.open(&fresh, OpenMode::CreateNew)?;
        out.write_at(0, &bytes)?;
        out.sync()?;
        drop(out);
        file.io.rename(&fresh, &file.path)?;
        file.io.sync_dir(file.path.parent().unwrap_or(Path::new(".")))
    }

    /// The bytes of the image of the store.
    pub(crate) fn image(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        VERSION.put(&mut out);
        {
            let state = self.state();
            let initdb = crate::system::catalog();
            let mut written: Vec<Arc<Catalog>> = Vec::new();
            state.databases.len().put(&mut out);
            for db in state.databases.values() {
                db.row.put(&mut out);
                if Arc::ptr_eq(&db.committed, &initdb) {
                    out.push(0);
                } else if let Some(at) = written.iter().position(|c| Arc::ptr_eq(c, &db.committed))
                {
                    out.push(1);
                    at.put(&mut out);
                } else {
                    out.push(2);
                    db.committed.put(&mut out);
                    written.push(Arc::clone(&db.committed));
                }
            }
            state.next_oid.put(&mut out);
            state.roles.put(&mut out);
            state.tablespaces.put(&mut out);
            state.settings.put(&mut out);
        }
        self.sequences.lock().unwrap_or_else(std::sync::PoisonError::into_inner).put(&mut out);
        self.heap.image(&mut out);
        out
    }

    /// Takes the databases, the roles, the sequences and the rows of an image.
    pub(crate) fn load(&mut self, bytes: &[u8]) -> Result<()> {
        let rest = bytes
            .strip_prefix(MAGIC.as_slice())
            .ok_or_else(|| corrupt("the file is not an image of rupg"))?;
        let mut input = Reader::new(rest);
        let version = u32::take(&mut input)?;
        if version != VERSION {
            return Err(Error::new(
                SqlState::DATA_CORRUPTED,
                format!(
                    "the image has the version {version}, and this server reads the version {VERSION}"
                ),
            ));
        }
        let initdb = crate::system::catalog();
        let mut read: Vec<Arc<Catalog>> = Vec::new();
        let mut state = State::default();
        state.databases.clear();
        for _ in 0..input.count()? {
            let row: rupg_catalog::Database = Persist::take(&mut input)?;
            let committed = match take_shared(&mut input)? {
                Shared::Initdb => Arc::clone(&initdb),
                Shared::Earlier(at) => Arc::clone(
                    read.get(at).ok_or_else(|| corrupt("a catalog is not in the image"))?,
                ),
                Shared::New => {
                    let catalog = Arc::new(Catalog::take(&mut input)?);
                    read.push(Arc::clone(&catalog));
                    catalog
                }
            };
            state.databases.insert(row.oid, Db { row, committed, locked: None });
        }
        state.next_oid = Persist::take(&mut input)?;
        state.roles = Persist::take(&mut input)?;
        state.tablespaces = Persist::take(&mut input)?;
        state.settings = Persist::take(&mut input)?;
        let sequences = Persist::take(&mut input)?;
        let heap = Heap::from_image(&mut input)?;
        if !input.is_empty() {
            return Err(corrupt("the image has bytes after its end"));
        }
        self.state = Mutex::new(state);
        self.sequences = Mutex::new(sequences);
        self.heap = heap;
        Ok(())
    }
}

fn take_shared(input: &mut Reader<'_>) -> Result<Shared> {
    match u8::take(input)? {
        0 => Ok(Shared::Initdb),
        1 => Ok(Shared::Earlier(Persist::take(input)?)),
        2 => Ok(Shared::New),
        _ => Err(corrupt("a catalog of the image has a bad tag")),
    }
}
