//! The `Io` and `File` traits. Every read, write and sync of the engine goes through them.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use rupg_common::{Error, Result, SqlState};

/// How [`Io::open`] opens a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// Open an existing file to read.
    Read,
    /// Open an existing file to read and write.
    ReadWrite,
    /// Open a file to read and write, and make it if it does not exist.
    Create,
    /// Make a new file to read and write. It is an error if the file exists.
    CreateNew,
}

impl OpenMode {
    /// True if the mode allows writes.
    pub fn writes(self) -> bool {
        self != OpenMode::Read
    }
}

/// The file system.
pub trait Io: Send + Sync + fmt::Debug {
    /// Opens a file.
    fn open(&self, path: &Path, mode: OpenMode) -> Result<Arc<dyn File>>;

    /// Removes a file.
    fn remove(&self, path: &Path) -> Result<()>;

    /// True if the file exists.
    fn exists(&self, path: &Path) -> Result<bool>;

    /// Makes the names in a directory durable, for example after a new file. Call it before the engine depends on the new file after a crash.
    fn sync_dir(&self, path: &Path) -> Result<()>;

    /// The names of the entries of a directory, in no order, each with true for a directory. The configuration files use it for `include_dir`.
    ///
    /// # Errors
    ///
    /// The directory cannot be read. An implementation without directories gives `0A000`.
    fn read_dir(&self, path: &Path) -> Result<Vec<(String, bool)>> {
        Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("could not open directory \"{}\": not supported", path.display()),
        ))
    }

    /// The whole contents of a file, for a small file such as a configuration file.
    ///
    /// # Errors
    ///
    /// The file cannot be opened or read.
    fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        let file = self.open(path, OpenMode::Read)?;
        let size = usize::try_from(file.size()?).map_err(|_| {
            Error::new(SqlState::OUT_OF_MEMORY, format!("file \"{}\" is too large", path.display()))
        })?;
        let mut data = vec![0; size];
        file.read_at(0, &mut data)?;
        Ok(data)
    }
}

/// An open file. The offsets are in bytes. A file can be used from many threads at the same time.
pub trait File: Send + Sync + fmt::Debug {
    /// Reads `buf.len()` bytes at `offset`. It is an error with SQLSTATE `XX001` if the file ends before.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Writes all of `data` at `offset`. The file grows if necessary.
    ///
    /// The write is not durable until the next [`File::sync`] completes. A power cut before then can lose it, tear it, or keep it (spec/21 section 21.9.1).
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<()>;

    /// Makes the completed writes durable, with `fdatasync` or the same call of the platform.
    ///
    /// An error from this call is fatal. The kernel can drop the dirty pages after the error, so a second sync can succeed with data lost. The caller must stop and recover from the log. It must not retry.
    fn sync(&self) -> Result<()>;

    /// The size of the file in bytes.
    fn size(&self) -> Result<u64>;

    /// Sets the size of the file. New bytes are zero. The change is durable after the next [`File::sync`].
    fn set_size(&self, size: u64) -> Result<()>;
}
