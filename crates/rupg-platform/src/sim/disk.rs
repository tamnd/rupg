//! A disk in memory with the power cut model of spec/21 section 21.9.1.
//!
//! Each file has two images. The durable image is what is on the disk for sure: the state at the last completed sync. The current image is what reads see. A write changes the current image and goes on the list of unsynced operations of the file. A sync makes the current image the durable image and clears the list. The images and the data of the operations are shared until a write changes them, so a fork and a sync copy no bytes.
//!
//! [`SimIo::crash`] is the power cut. It builds each file from its durable image and a [`CrashPlan`], which says for each unsynced operation whether it reached the disk, was lost, was torn or went to a different offset. A test can list the unsynced operations with [`SimIo::unsynced`], build every plan that it wants to try, and use [`SimIo::fork`] to try each one on its own copy of the disk.
//!
//! Making and removing a file is durable at once. The model does not test the directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use rupg_common::{Error, Result, SqlState};

use super::{SimRng, lock};
use crate::{File, Io, OpenMode};

/// The faults that the simulated disk injects. The rates are in parts per million of the calls. The default injects no fault.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Faults {
    /// The rate of `EIO` on a read.
    pub read_error: u32,
    /// The rate of `EIO` on a write.
    pub write_error: u32,
    /// The rate of `EIO` on a sync. After this fault, each write and sync of the file fails until the next crash, as the engine must stop.
    pub sync_error: u32,
    /// The size of the disk in bytes, for all files together. A write or a size change past it fails with `ENOSPC`.
    pub capacity: Option<u64>,
}

/// An operation since the last sync of its file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unsynced {
    /// A write of `len` bytes at `offset`.
    Write {
        /// The file.
        path: PathBuf,
        /// The offset of the first byte.
        offset: u64,
        /// The number of bytes.
        len: u64,
    },
    /// A change of the file size.
    SetSize {
        /// The file.
        path: PathBuf,
        /// The new size.
        size: u64,
    },
}

impl Unsynced {
    /// The file.
    pub fn path(&self) -> &Path {
        match self {
            Unsynced::Write { path, .. } | Unsynced::SetSize { path, .. } => path,
        }
    }

    /// The places where a disk with blocks of `block` bytes can tear this write, as the number of bytes that reach the disk. The disk writes whole blocks, so each tear is at a multiple of `block` from the start of the file. The list is empty for a size change and for a write inside one block.
    pub fn tears(&self, block: u64) -> Vec<u64> {
        let Unsynced::Write { offset, len, .. } = *self else {
            return Vec::new();
        };
        if block == 0 {
            return Vec::new();
        }
        let first = (offset / block + 1) * block;
        (first..offset + len).step_by(block as usize).map(|b| b - offset).collect()
    }
}

/// What a power cut does to one unsynced operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// The operation did not reach the disk.
    Lost,
    /// The operation reached the disk in full.
    Written,
    /// Only the first `keep` bytes of the write reached the disk. A size change with this fate is lost.
    Torn {
        /// The number of bytes that reached the disk.
        keep: u64,
    },
    /// The write reached the disk at `offset` and not at its own offset. Firmware bugs do this. A size change with this fate is lost.
    Misdirected {
        /// The offset where the bytes went.
        offset: u64,
    },
}

/// The fate of each unsynced operation and the order in which they reach the disk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CrashPlan {
    /// One fate for each entry of [`SimIo::unsynced`], in the same order.
    pub fates: Vec<Fate>,
    /// The order in which the operations reach the disk, as indexes into `fates`. Empty means the issue order. The order matters only when two writes to a file overlap.
    pub order: Vec<usize>,
}

#[derive(Clone, Debug)]
enum Op {
    Write { offset: u64, data: Arc<[u8]> },
    SetSize(u64),
}

#[derive(Clone, Debug, Default)]
struct FileState {
    durable: Image,
    current: Image,
    pending: Vec<Op>,
    /// A sync failed. Writes and syncs fail until the next crash.
    broken: bool,
}

#[derive(Clone, Debug)]
struct Disk {
    files: BTreeMap<PathBuf, FileState>,
    faults: Faults,
    rng: SimRng,
    /// Goes up at each crash. A handle from an older generation fails.
    generation: u64,
    syncs: u64,
}

/// Checks that a file can grow from `old` to `new` bytes when all files use `used` bytes.
fn room(path: &Path, faults: Faults, used: u64, old: u64, new: u64) -> Result<()> {
    match faults.capacity {
        Some(capacity) if new > old && used - old + new > capacity => Err(Error::new(
            SqlState::DISK_FULL,
            format!(
                "could not write to file \"{}\": no space left on the simulated disk",
                path.display()
            ),
        )),
        _ => Ok(()),
    }
}

fn eio(action: &str, path: &Path) -> Error {
    Error::new(
        SqlState::IO_ERROR,
        format!("could not {action} file \"{}\": simulated EIO", path.display()),
    )
}

fn index(n: u64, path: &Path) -> Result<usize> {
    usize::try_from(n).map_err(|_| {
        Error::new(
            SqlState::IO_ERROR,
            format!(
                "offset {n} of file \"{}\" is too large for the simulated disk",
                path.display()
            ),
        )
    })
}

/// The size of a chunk of an image.
const CHUNK: usize = 1 << 16;

/// The bytes of a file, in chunks of [`CHUNK`] bytes that the copies share until a write changes them. A fork, a sync and a power cut copy no bytes, and a write copies only the chunks that it changes. The bytes of the last chunk after the length are zero.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Image {
    len: usize,
    chunks: Vec<Arc<Vec<u8>>>,
}

/// The chunk of zeros that each new chunk shares.
fn zero_chunk() -> Arc<Vec<u8>> {
    static ZERO: OnceLock<Arc<Vec<u8>>> = OnceLock::new();
    Arc::clone(ZERO.get_or_init(|| Arc::new(vec![0; CHUNK])))
}

impl Image {
    fn len(&self) -> usize {
        self.len
    }

    /// Sets the length. New bytes are zero.
    fn set_len(&mut self, len: usize) {
        if len < self.len {
            self.chunks.truncate(len.div_ceil(CHUNK));
            if !len.is_multiple_of(CHUNK)
                && let Some(last) = self.chunks.last_mut()
            {
                Arc::make_mut(last)[len % CHUNK..].fill(0);
            }
        } else {
            self.chunks.resize_with(len.div_ceil(CHUNK), zero_chunk);
        }
        self.len = len;
    }

    fn write(&mut self, offset: usize, data: &[u8]) {
        let end = offset + data.len();
        if self.len < end {
            self.set_len(end);
        }
        let mut at = offset;
        while at < end {
            let (chunk, from) = (at / CHUNK, at % CHUNK);
            let n = (CHUNK - from).min(end - at);
            Arc::make_mut(&mut self.chunks[chunk])[from..from + n]
                .copy_from_slice(&data[at - offset..at - offset + n]);
            at += n;
        }
    }

    /// Reads `buf.len()` bytes at `offset`. The result is false if the image ends before.
    fn read(&self, offset: usize, buf: &mut [u8]) -> bool {
        if offset.checked_add(buf.len()).is_none_or(|end| end > self.len) {
            return false;
        }
        let mut done = 0;
        while done < buf.len() {
            let (chunk, from) = ((offset + done) / CHUNK, (offset + done) % CHUNK);
            let n = (CHUNK - from).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&self.chunks[chunk][from..from + n]);
            done += n;
        }
        true
    }

    /// The pieces of the image in order, with no copy.
    fn pieces(&self) -> impl Iterator<Item = &[u8]> {
        let len = self.len;
        self.chunks.iter().enumerate().map(move |(i, c)| &c[..(len - i * CHUNK).min(CHUNK)])
    }

    fn to_vec(&self) -> Vec<u8> {
        self.pieces().flatten().copied().collect()
    }
}

/// A file system in memory with a power cut model and fault injection.
#[derive(Clone, Debug)]
pub struct SimIo {
    disk: Arc<Mutex<Disk>>,
}

impl SimIo {
    /// An empty disk. The seed drives the faults and [`SimIo::crash_random`].
    pub fn new(seed: u64) -> SimIo {
        SimIo {
            disk: Arc::new(Mutex::new(Disk {
                files: BTreeMap::new(),
                faults: Faults::default(),
                rng: SimRng::new(seed),
                generation: 0,
                syncs: 0,
            })),
        }
    }

    /// Sets the faults.
    pub fn set_faults(&self, faults: Faults) {
        lock(&self.disk).faults = faults;
    }

    /// A copy of the disk with its own state. The open handles stay with this disk.
    pub fn fork(&self) -> SimIo {
        SimIo { disk: Arc::new(Mutex::new(lock(&self.disk).clone())) }
    }

    /// The number of syncs that completed.
    pub fn syncs(&self) -> u64 {
        lock(&self.disk).syncs
    }

    /// The bytes that reads of the file see now.
    pub fn contents(&self, path: &Path) -> Option<Vec<u8>> {
        lock(&self.disk).files.get(path).map(|f| f.current.to_vec())
    }

    /// Calls `f` on the bytes that reads of the file see now, in order and in pieces, with no copy. The result is false if the file does not exist.
    pub fn read_pieces(&self, path: &Path, mut f: impl FnMut(&[u8])) -> bool {
        let Some(image) = lock(&self.disk).files.get(path).map(|f| f.current.clone()) else {
            return false;
        };
        image.pieces().for_each(&mut f);
        true
    }

    /// The bytes of the file that are on the disk for sure.
    pub fn durable(&self, path: &Path) -> Option<Vec<u8>> {
        lock(&self.disk).files.get(path).map(|f| f.durable.to_vec())
    }

    /// The operations since the last sync of each file: the files in path order, and the operations of a file in issue order.
    pub fn unsynced(&self) -> Vec<Unsynced> {
        let disk = lock(&self.disk);
        let mut list = Vec::new();
        for (path, file) in &disk.files {
            for op in &file.pending {
                list.push(match op {
                    Op::Write { offset, data } => Unsynced::Write {
                        path: path.clone(),
                        offset: *offset,
                        len: data.len() as u64,
                    },
                    Op::SetSize(size) => Unsynced::SetSize { path: path.clone(), size: *size },
                });
            }
        }
        list
    }

    /// Cuts the power. Each file gets its durable image with the operations that the plan lets through. The handles that are open now fail from this call on, as the process that held them is dead.
    pub fn crash(&self, plan: &CrashPlan) -> Result<()> {
        let mut disk = lock(&self.disk);
        let ops: Vec<(PathBuf, Op)> = disk
            .files
            .iter()
            .flat_map(|(path, file)| file.pending.iter().map(move |op| (path.clone(), op.clone())))
            .collect();
        if plan.fates.len() != ops.len() {
            return Err(Error::internal(format!(
                "the crash plan has {} fates for {} unsynced operations",
                plan.fates.len(),
                ops.len()
            )));
        }
        let order: Vec<usize> =
            if plan.order.is_empty() { (0..ops.len()).collect() } else { plan.order.clone() };
        let mut sorted = order.clone();
        sorted.sort_unstable();
        if sorted != (0..ops.len()).collect::<Vec<_>>() {
            return Err(Error::internal("the order of the crash plan is not a permutation"));
        }

        let mut images: BTreeMap<PathBuf, Image> =
            disk.files.iter().map(|(p, f)| (p.clone(), f.durable.clone())).collect();
        for i in order {
            let (path, op) = &ops[i];
            let Some(image) = images.get_mut(path) else { continue };
            match (op, plan.fates[i]) {
                (_, Fate::Lost) => {}
                (Op::SetSize(size), Fate::Written) => {
                    image.set_len(index(*size, path)?);
                }
                (Op::SetSize(_), _) => {}
                (Op::Write { offset, data }, Fate::Written) => {
                    image.write(index(*offset, path)?, data);
                }
                (Op::Write { offset, data }, Fate::Torn { keep }) => {
                    let keep = index(keep, path)?.min(data.len());
                    image.write(index(*offset, path)?, &data[..keep]);
                }
                (Op::Write { data, .. }, Fate::Misdirected { offset }) => {
                    image.write(index(offset, path)?, data);
                }
            }
        }
        for (path, image) in images {
            let Some(file) = disk.files.get_mut(&path) else { continue };
            file.current = image.clone();
            file.durable = image;
            file.pending.clear();
            file.broken = false;
        }
        disk.generation += 1;
        Ok(())
    }

    /// Cuts the power with a plan from the seeded generator, and gives the plan. Each operation is lost, written or torn at a 4 KiB or 512 byte boundary, and the operations reach the disk in a random order. The plan has no misdirected write.
    pub fn crash_random(&self) -> Result<CrashPlan> {
        let unsynced = self.unsynced();
        let plan = {
            let mut disk = lock(&self.disk);
            let rng = &mut disk.rng;
            let fates = unsynced
                .iter()
                .map(|op| match rng.below(3) {
                    0 => Fate::Lost,
                    1 => Fate::Written,
                    _ => {
                        let block = if rng.below(2) == 0 { 4096 } else { 512 };
                        let tears = op.tears(block);
                        if tears.is_empty() {
                            Fate::Written
                        } else {
                            Fate::Torn { keep: tears[rng.below(tears.len() as u64) as usize] }
                        }
                    }
                })
                .collect();
            let mut order: Vec<usize> = (0..unsynced.len()).collect();
            rng.shuffle(&mut order);
            CrashPlan { fates, order }
        };
        self.crash(&plan)?;
        Ok(plan)
    }
}

impl Io for SimIo {
    fn open(&self, path: &Path, mode: OpenMode) -> Result<Arc<dyn File>> {
        let mut disk = lock(&self.disk);
        let exists = disk.files.contains_key(path);
        match mode {
            OpenMode::Read | OpenMode::ReadWrite if !exists => {
                return Err(Error::new(
                    SqlState::UNDEFINED_FILE,
                    format!("could not open file \"{}\": no such file", path.display()),
                ));
            }
            OpenMode::CreateNew if exists => {
                return Err(Error::new(
                    SqlState::DUPLICATE_FILE,
                    format!("could not open file \"{}\": the file exists", path.display()),
                ));
            }
            _ => {}
        }
        disk.files.entry(path.to_path_buf()).or_default();
        Ok(Arc::new(SimFile {
            disk: Arc::clone(&self.disk),
            path: path.to_path_buf(),
            generation: disk.generation,
            writable: mode.writes(),
        }))
    }

    fn remove(&self, path: &Path) -> Result<()> {
        match lock(&self.disk).files.remove(path) {
            Some(_) => Ok(()),
            None => Err(Error::new(
                SqlState::UNDEFINED_FILE,
                format!("could not remove file \"{}\": no such file", path.display()),
            )),
        }
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        Ok(lock(&self.disk).files.contains_key(path))
    }

    fn sync_dir(&self, _path: &Path) -> Result<()> {
        Ok(())
    }
}

/// A file of [`SimIo`].
#[derive(Debug)]
pub struct SimFile {
    disk: Arc<Mutex<Disk>>,
    path: PathBuf,
    generation: u64,
    writable: bool,
}

impl SimFile {
    /// Runs `f` on the state of the file, after the checks that every call makes.
    fn with<T>(
        &self,
        action: &str,
        f: impl FnOnce(&mut FileState, &mut SimRng, Faults, u64) -> Result<T>,
    ) -> Result<T> {
        let mut disk = lock(&self.disk);
        if disk.generation != self.generation {
            return Err(Error::new(
                SqlState::IO_ERROR,
                format!(
                    "could not {action} file \"{}\": the handle is from before a simulated crash",
                    self.path.display()
                ),
            ));
        }
        let disk = &mut *disk;
        let used = disk.files.values().map(|f| f.current.len() as u64).sum();
        let Some(file) = disk.files.get_mut(&self.path) else {
            return Err(Error::new(
                SqlState::UNDEFINED_FILE,
                format!(
                    "could not {action} file \"{}\": the file was removed",
                    self.path.display()
                ),
            ));
        };
        f(file, &mut disk.rng, disk.faults, used)
    }

    fn check_write(&self, action: &str, file: &FileState) -> Result<()> {
        if !self.writable {
            return Err(Error::new(
                SqlState::IO_ERROR,
                format!("could not {action} file \"{}\": open to read only", self.path.display()),
            ));
        }
        if file.broken {
            return Err(Error::new(
                SqlState::IO_ERROR,
                format!(
                    "could not {action} file \"{}\": an earlier sync failed",
                    self.path.display()
                ),
            ));
        }
        Ok(())
    }
}

impl File for SimFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.with("read", |file, rng, faults, _| {
            if rng.chance(faults.read_error) {
                return Err(eio("read", &self.path));
            }
            let start = index(offset, &self.path)?;
            if file.current.read(start, buf) {
                return Ok(());
            }
            Err(Error::corrupted(format!(
                "could not read {} bytes at offset {offset} of file \"{}\": the file is shorter",
                buf.len(),
                self.path.display()
            )))
        })
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        let start = index(offset, &self.path)?;
        self.with("write to", |file, rng, faults, used| {
            self.check_write("write to", file)?;
            let old = file.current.len() as u64;
            room(&self.path, faults, used, old, old.max(offset + data.len() as u64))?;
            if rng.chance(faults.write_error) {
                return Err(eio("write to", &self.path));
            }
            file.current.write(start, data);
            file.pending.push(Op::Write { offset, data: data.into() });
            Ok(())
        })
    }

    fn sync(&self) -> Result<()> {
        let synced = self.with("fdatasync", |file, rng, faults, _| {
            self.check_write("fdatasync", file)?;
            if rng.chance(faults.sync_error) {
                file.broken = true;
                return Err(eio("fdatasync", &self.path));
            }
            file.durable = file.current.clone();
            file.pending.clear();
            Ok(())
        });
        if synced.is_ok() {
            lock(&self.disk).syncs += 1;
        }
        synced
    }

    fn size(&self) -> Result<u64> {
        self.with("stat", |file, _, _, _| Ok(file.current.len() as u64))
    }

    fn set_size(&self, size: u64) -> Result<()> {
        let new = index(size, &self.path)?;
        self.with("truncate", |file, _, faults, used| {
            self.check_write("truncate", file)?;
            room(&self.path, faults, used, file.current.len() as u64, size)?;
            file.current.set_len(new);
            file.pending.push(Op::SetSize(size));
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_shares_its_chunks() {
        let mut a = Image::default();
        a.write(CHUNK - 3, &[7; 6]);
        let b = a.clone();
        a.write(1, &[9]);
        // The copy keeps its bytes, and the chunk that the write did not change is shared.
        assert_eq!(b.to_vec()[1], 0);
        assert_eq!(a.to_vec()[1], 9);
        assert!(Arc::ptr_eq(&a.chunks[1], &b.chunks[1]));
        let mut buf = [0; 6];
        assert!(a.read(CHUNK - 3, &mut buf));
        assert_eq!(buf, [7; 6]);
        assert!(!a.read(CHUNK, &mut buf));
        // Bytes after a shrink are zero when the image grows again.
        a.set_len(CHUNK - 1);
        a.set_len(2 * CHUNK + 5);
        assert!(a.read(CHUNK - 3, &mut buf));
        assert_eq!(buf, [7, 7, 0, 0, 0, 0]);
        assert_eq!(a.pieces().map(<[u8]>::len).collect::<Vec<_>>(), [CHUNK, CHUNK, 5]);
    }

    fn path() -> PathBuf {
        PathBuf::from("/sim/a.rupg")
    }

    /// A disk with one synced block of `a` and two unsynced writes of 8 KiB: `b` at 4096 and `c` at 0.
    fn setup() -> (SimIo, Arc<dyn File>) {
        let io = SimIo::new(1);
        let f = io.open(&path(), OpenMode::CreateNew).unwrap();
        f.write_at(0, &[b'a'; 4096]).unwrap();
        f.sync().unwrap();
        f.write_at(4096, &[b'b'; 8192]).unwrap();
        f.write_at(0, &[b'c'; 8192]).unwrap();
        (io, f)
    }

    #[test]
    fn reads_see_the_writes() {
        let (io, f) = setup();
        let mut buf = [0; 4];
        f.read_at(8190, &mut buf).unwrap();
        assert_eq!(&buf, b"ccbb");
        assert_eq!(f.size().unwrap(), 12288);
        assert_eq!(io.durable(&path()).unwrap(), vec![b'a'; 4096]);
        assert_eq!(io.syncs(), 1);
        let e = f.read_at(12286, &mut buf).unwrap_err();
        assert_eq!(e.state(), SqlState::DATA_CORRUPTED);
    }

    #[test]
    fn the_unsynced_list_and_the_tears() {
        let (io, _f) = setup();
        let list = io.unsynced();
        assert_eq!(
            list,
            [
                Unsynced::Write { path: path(), offset: 4096, len: 8192 },
                Unsynced::Write { path: path(), offset: 0, len: 8192 },
            ]
        );
        assert_eq!(list[0].tears(4096), [4096]);
        assert_eq!(list[0].tears(512).len(), 15);
        let odd = Unsynced::Write { path: path(), offset: 100, len: 5000 };
        assert_eq!(odd.tears(4096), [3996]);
        assert!(Unsynced::Write { path: path(), offset: 10, len: 20 }.tears(4096).is_empty());
    }

    #[test]
    fn a_crash_keeps_only_what_the_plan_lets_through() {
        let (io, f) = setup();
        let lost = io.fork();
        lost.crash(&CrashPlan { fates: vec![Fate::Lost, Fate::Lost], order: vec![] }).unwrap();
        assert_eq!(lost.contents(&path()).unwrap(), vec![b'a'; 4096]);

        // Both written, the second first. Then the first write wins where they overlap.
        let both = io.fork();
        let plan = CrashPlan { fates: vec![Fate::Written, Fate::Written], order: vec![1, 0] };
        both.crash(&plan).unwrap();
        let image = both.contents(&path()).unwrap();
        assert_eq!(&image[..4096], &[b'c'; 4096]);
        assert_eq!(&image[4096..], &[b'b'; 8192]);

        let torn = io.fork();
        let plan = CrashPlan { fates: vec![Fate::Lost, Fate::Torn { keep: 512 }], order: vec![] };
        torn.crash(&plan).unwrap();
        let image = torn.contents(&path()).unwrap();
        assert_eq!(image.len(), 4096);
        assert_eq!(&image[..512], &[b'c'; 512]);
        assert_eq!(&image[512..], &[b'a'; 3584]);

        let wrong = io.fork();
        let plan = CrashPlan {
            fates: vec![Fate::Lost, Fate::Misdirected { offset: 16384 }],
            order: vec![],
        };
        wrong.crash(&plan).unwrap();
        let image = wrong.contents(&path()).unwrap();
        assert_eq!(&image[..4096], &[b'a'; 4096]);
        assert_eq!(&image[16384..], &[b'c'; 8192]);

        // The fork did not change the first disk, and its handle still works.
        assert_eq!(io.unsynced().len(), 2);
        f.write_at(0, b"x").unwrap();
    }

    #[test]
    fn a_handle_from_before_the_crash_fails() {
        let (io, f) = setup();
        io.crash(&CrashPlan { fates: vec![Fate::Lost; 2], order: vec![] }).unwrap();
        assert!(f.read_at(0, &mut [0; 1]).is_err());
        assert!(io.unsynced().is_empty());
        let f = io.open(&path(), OpenMode::ReadWrite).unwrap();
        f.write_at(0, b"x").unwrap();
    }

    #[test]
    fn a_bad_plan_is_an_error() {
        let (io, _f) = setup();
        assert!(io.crash(&CrashPlan { fates: vec![Fate::Lost], order: vec![] }).is_err());
        let plan = CrashPlan { fates: vec![Fate::Lost; 2], order: vec![0, 0] };
        assert!(io.crash(&plan).is_err());
    }

    #[test]
    fn a_size_change_is_an_operation() {
        let io = SimIo::new(1);
        let f = io.open(&path(), OpenMode::Create).unwrap();
        f.set_size(16384).unwrap();
        assert_eq!(f.size().unwrap(), 16384);
        assert!(matches!(io.unsynced()[0], Unsynced::SetSize { size: 16384, .. }));
        let kept = io.fork();
        kept.crash(&CrashPlan { fates: vec![Fate::Written], order: vec![] }).unwrap();
        assert_eq!(kept.contents(&path()).unwrap().len(), 16384);
        io.crash(&CrashPlan { fates: vec![Fate::Torn { keep: 1 }], order: vec![] }).unwrap();
        assert_eq!(io.contents(&path()).unwrap().len(), 0);
    }

    #[test]
    fn a_random_crash_is_the_same_for_the_same_seed() {
        let run = |seed| {
            let io = SimIo::new(seed);
            let f = io.open(&path(), OpenMode::CreateNew).unwrap();
            for i in 0..20u8 {
                f.write_at(u64::from(i) * 3000, &[i; 9000]).unwrap();
            }
            let plan = io.crash_random().unwrap();
            (plan, io.contents(&path()).unwrap())
        };
        assert_eq!(run(5), run(5));
        assert_ne!(run(5).1, run(6).1);
        let (plan, _) = run(5);
        assert!(plan.fates.iter().any(|f| matches!(f, Fate::Torn { .. })));
    }

    #[test]
    fn the_faults() {
        let io = SimIo::new(3);
        let f = io.open(&path(), OpenMode::Create).unwrap();
        io.set_faults(Faults { capacity: Some(10_000), ..Faults::default() });
        f.write_at(0, &[1; 8000]).unwrap();
        let e = f.write_at(8000, &[1; 4000]).unwrap_err();
        assert_eq!(e.state(), SqlState::DISK_FULL);
        assert_eq!(f.set_size(20_000).unwrap_err().state(), SqlState::DISK_FULL);
        // A write inside the file needs no new space.
        f.write_at(0, &[2; 8000]).unwrap();

        io.set_faults(Faults { sync_error: 1_000_000, ..Faults::default() });
        assert_eq!(f.sync().unwrap_err().state(), SqlState::IO_ERROR);
        io.set_faults(Faults::default());
        // The engine must stop after a failed sync, so the file refuses more work.
        assert!(f.sync().is_err());
        assert!(f.write_at(0, b"x").is_err());
        let n = io.unsynced().len();
        io.crash(&CrashPlan { fates: vec![Fate::Lost; n], order: vec![] }).unwrap();
        let f = io.open(&path(), OpenMode::ReadWrite).unwrap();
        f.write_at(0, b"x").unwrap();
        f.sync().unwrap();

        io.set_faults(Faults {
            read_error: 1_000_000,
            write_error: 1_000_000,
            ..Faults::default()
        });
        assert_eq!(f.read_at(0, &mut [0; 1]).unwrap_err().state(), SqlState::IO_ERROR);
        assert_eq!(f.write_at(0, b"y").unwrap_err().state(), SqlState::IO_ERROR);
    }

    #[test]
    fn open_modes() {
        let io = SimIo::new(1);
        assert_eq!(
            io.open(&path(), OpenMode::ReadWrite).unwrap_err().state(),
            SqlState::UNDEFINED_FILE
        );
        io.open(&path(), OpenMode::CreateNew).unwrap();
        assert_eq!(
            io.open(&path(), OpenMode::CreateNew).unwrap_err().state(),
            SqlState::DUPLICATE_FILE
        );
        let r = io.open(&path(), OpenMode::Read).unwrap();
        assert!(r.write_at(0, b"x").is_err());
        assert!(io.exists(&path()).unwrap());
        io.remove(&path()).unwrap();
        assert!(!io.exists(&path()).unwrap());
        assert_eq!(r.size().unwrap_err().state(), SqlState::UNDEFINED_FILE);
        assert!(io.remove(&path()).is_err());
    }
}
