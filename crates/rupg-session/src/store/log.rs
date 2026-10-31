//! The log of the commits of a store, the `-log-N` files next to the image. Each commit after a checkpoint adds the bytes of its changes to the log, and the commit ends when the log is on the disk. A restart loads the image and makes the changes of the logs again, so a commit stays after a crash. The commits that wait for the disk at the same time share one sync, as the group commit of `XLogFlush` does.
//!
//! A checkpoint starts a new log, the next `N`, and then writes the image with that `N`: a restart reads the logs from the `N` of the image on, while they exist. So a crash before the new image is on the disk finds the old image and both logs.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use rupg_common::Result;
use rupg_common::persist::corrupt;
use rupg_kernels::crc32c;
use rupg_platform::{File, Io, OpenMode, Tasks};

/// The first bytes of a log, before the number of the log.
const MAGIC: &[u8; 8] = b"rupg\x00log";
/// The bytes of the head of a log: the magic and the number.
const HEAD: u64 = 16;
/// The bytes before the body of a record: its length and the CRC-32C of the body.
const FRAME: usize = 8;

/// The file of the log with the number `epoch` of the image `image`.
pub(super) fn path(image: &Path, epoch: u64) -> PathBuf {
    let mut name = image.as_os_str().to_owned();
    name.push(format!("-log-{epoch}"));
    PathBuf::from(name)
}

/// The log that the commits write now.
pub(crate) struct Log {
    io: Arc<dyn Io>,
    image: PathBuf,
    inner: Mutex<Inner>,
    synced: Condvar,
}

struct Inner {
    /// The number of the log.
    epoch: u64,
    file: Arc<dyn File>,
    /// The end of the bytes that went to the file.
    end: u64,
    /// The records that did not go to the file yet.
    pending: Vec<u8>,
    /// The number of the records that the commits added.
    added: u64,
    /// The number of the records that are on the disk.
    synced: u64,
    /// True while a commit writes the pending records and syncs the file.
    syncing: bool,
}

impl fmt::Debug for Log {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.lock();
        f.debug_struct("Log").field("image", &self.image).field("epoch", &inner.epoch).finish()
    }
}

/// Makes the new log `epoch`, with its head on the disk.
fn create(io: &dyn Io, image: &Path, epoch: u64) -> Result<Arc<dyn File>> {
    let at = path(image, epoch);
    if io.exists(&at)? {
        io.remove(&at)?;
    }
    let file = io.open(&at, OpenMode::CreateNew)?;
    let mut head = MAGIC.to_vec();
    head.extend_from_slice(&epoch.to_le_bytes());
    file.write_at(0, &head)?;
    file.sync()?;
    io.sync_dir(image.parent().unwrap_or(Path::new(".")))?;
    Ok(file)
}

impl Log {
    /// The new log `epoch` of the image `image`.
    ///
    /// # Errors
    ///
    /// The error of the file.
    pub(super) fn create(io: Arc<dyn Io>, image: PathBuf, epoch: u64) -> Result<Log> {
        let file = create(&*io, &image, epoch)?;
        let inner = Inner {
            epoch,
            file,
            end: HEAD,
            pending: Vec::new(),
            added: 0,
            synced: 0,
            syncing: false,
        };
        Ok(Log { io, image, inner: Mutex::new(inner), synced: Condvar::new() })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds the record that `body` writes, and gives its number for [`Log::wait`]. `body` runs while the log is locked, so the records are in the order of the calls.
    pub(super) fn add(&self, body: &mut dyn FnMut(&mut Vec<u8>)) -> u64 {
        let mut inner = self.lock();
        let start = inner.pending.len();
        inner.pending.extend_from_slice(&[0; FRAME]);
        body(&mut inner.pending);
        let record = &inner.pending[start + FRAME..];
        let len = u32::try_from(record.len()).unwrap_or(u32::MAX);
        let crc = crc32c::extend(0, record);
        inner.pending[start..start + 4].copy_from_slice(&len.to_le_bytes());
        inner.pending[start + 4..start + FRAME].copy_from_slice(&crc.to_le_bytes());
        inner.added += 1;
        inner.added
    }

    /// The bytes of the log on the disk.
    pub(super) fn size(&self) -> u64 {
        self.lock().end
    }

    /// The number of the last record.
    pub(super) fn added(&self) -> u64 {
        self.lock().added
    }

    /// Waits until the record `number` is on the disk. The first commit that waits writes and syncs the records of all the commits before it, and the others wait for its sync.
    ///
    /// # Errors
    ///
    /// The error of a write or a sync of the file.
    pub(super) fn wait(&self, number: u64, tasks: Option<&dyn Tasks>) -> Result<()> {
        let mut inner = self.lock();
        while inner.synced < number {
            if inner.syncing {
                inner = match tasks {
                    Some(tasks) => {
                        drop(inner);
                        tasks.yield_now();
                        self.lock()
                    }
                    None => self.synced.wait(inner).unwrap_or_else(PoisonError::into_inner),
                };
                continue;
            }
            inner.syncing = true;
            let bytes = std::mem::take(&mut inner.pending);
            let (file, at, upto) = (Arc::clone(&inner.file), inner.end, inner.added);
            inner.end += bytes.len() as u64;
            drop(inner);
            let result = file.write_at(at, &bytes).and_then(|()| file.sync());
            inner = self.lock();
            inner.syncing = false;
            if result.is_ok() {
                inner.synced = inner.synced.max(upto);
            }
            self.synced.notify_all();
            result?;
        }
        Ok(())
    }

    /// Starts the next log for a checkpoint, after the records of this log are on the disk, and gives its number. The image of the checkpoint has the changes of the records of this log.
    ///
    /// # Errors
    ///
    /// The error of a write or a sync of a file.
    pub(super) fn next(&self, tasks: Option<&dyn Tasks>) -> Result<u64> {
        let mut inner = self.lock();
        while inner.syncing {
            inner = match tasks {
                Some(tasks) => {
                    drop(inner);
                    tasks.yield_now();
                    self.lock()
                }
                None => self.synced.wait(inner).unwrap_or_else(PoisonError::into_inner),
            };
        }
        if !inner.pending.is_empty() {
            inner.file.write_at(inner.end, &inner.pending)?;
            inner.file.sync()?;
            inner.pending.clear();
        }
        let epoch = inner.epoch + 1;
        inner.file = create(&*self.io, &self.image, epoch)?;
        inner.epoch = epoch;
        inner.end = HEAD;
        inner.synced = inner.added;
        self.synced.notify_all();
        Ok(epoch)
    }

    /// Removes the logs from `from` up to the log before `to`, whose changes the image of a checkpoint has.
    ///
    /// # Errors
    ///
    /// The error of a removal.
    pub(super) fn remove(&self, from: u64, to: u64) -> Result<()> {
        for epoch in from..to {
            let at = path(&self.image, epoch);
            if self.io.exists(&at)? {
                self.io.remove(&at)?;
            }
        }
        Ok(())
    }
}

/// The records of the log `epoch` of the image `image`, or `None` when the log does not exist. The records end at the first record that is cut or whose CRC does not match, which a crash during its write left.
///
/// # Errors
///
/// The error of a read, and `XX001` for a file that is not a log.
pub(super) fn read(io: &dyn Io, image: &Path, epoch: u64) -> Result<Option<Vec<Vec<u8>>>> {
    let at = path(image, epoch);
    if !io.exists(&at)? {
        return Ok(None);
    }
    let file = io.open(&at, OpenMode::Read)?;
    let len = usize::try_from(file.size()?).map_err(|_| corrupt("the log is too large"))?;
    let mut bytes = vec![0; len];
    file.read_at(0, &mut bytes)?;
    // A crash after the new file and before the sync of its head leaves a log with no records.
    let Some(head) = bytes.get(..16) else {
        return Ok(Some(Vec::new()));
    };
    if &head[..8] != MAGIC || head[8..] != epoch.to_le_bytes() {
        return Err(corrupt("the file is not a log of rupg"));
    }
    let mut records = Vec::new();
    let mut rest = &bytes[16..];
    while rest.len() >= FRAME {
        let len = u32::from_le_bytes(rest[..4].try_into().unwrap_or_default()) as usize;
        let crc = u32::from_le_bytes(rest[4..FRAME].try_into().unwrap_or_default());
        let Some(record) = rest.get(FRAME..FRAME + len) else { break };
        if crc32c::extend(0, record) != crc {
            break;
        }
        records.push(record.to_vec());
        rest = &rest[FRAME + len..];
    }
    Ok(Some(records))
}
