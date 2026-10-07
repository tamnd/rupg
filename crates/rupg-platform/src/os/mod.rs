//! The operating system implementations of the traits.

// This module is the one place where the engine may call the clock, the file system, the threads and the network of the operating system.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[cfg(unix)]
mod lookup;
mod net;
mod tasks;
#[cfg(unix)]
mod unix;

pub use net::OsNet;
pub use tasks::{OsTasks, STACK_SIZE};

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rupg_common::{Error, Result, SqlState};

use crate::{Clock, Entropy, File, FileMode, Io, OpenMode};
#[cfg(test)]
use crate::{Net, Tasks};

/// Converts an error of the operating system. The SQLSTATE is the one that PostgreSQL gives for the same cause.
fn os_error(e: &io::Error, action: &str, path: &Path) -> Error {
    let state = match e.kind() {
        io::ErrorKind::NotFound => SqlState::UNDEFINED_FILE,
        io::ErrorKind::AlreadyExists => SqlState::DUPLICATE_FILE,
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => SqlState::DISK_FULL,
        io::ErrorKind::OutOfMemory => SqlState::OUT_OF_MEMORY,
        _ => SqlState::IO_ERROR,
    };
    Error::new(state, format!("could not {action} file \"{}\": {}", path.display(), os_text(e)))
}

/// The text of an error of the operating system as `%m` gives it in PostgreSQL, without the code that Rust adds.
pub(crate) fn os_text(e: &io::Error) -> String {
    let text = e.to_string();
    match text.rfind(" (os error ") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

/// The file system of the operating system.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsIo;

impl Io for OsIo {
    fn open(&self, path: &Path, mode: OpenMode) -> Result<Arc<dyn File>> {
        let mut options = fs::OpenOptions::new();
        options.read(true).write(mode.writes());
        match mode {
            OpenMode::Read | OpenMode::ReadWrite => {}
            OpenMode::Create => {
                options.create(true);
            }
            OpenMode::CreateNew => {
                options.create_new(true);
            }
        }
        let file = options.open(path).map_err(|e| os_error(&e, "open", path))?;
        Ok(Arc::new(OsFile { file, path: path.to_path_buf() }))
    }

    fn remove(&self, path: &Path) -> Result<()> {
        fs::remove_file(path).map_err(|e| os_error(&e, "remove", path))
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        path.try_exists().map_err(|e| os_error(&e, "stat", path))
    }

    #[cfg(unix)]
    fn sync_dir(&self, path: &Path) -> Result<()> {
        let dir = fs::File::open(path).map_err(|e| os_error(&e, "open", path))?;
        dir.sync_all().map_err(|e| os_error(&e, "fsync", path))
    }

    #[cfg(not(unix))]
    fn sync_dir(&self, _path: &Path) -> Result<()> {
        // Windows makes the names durable with the file. It has no call to sync a directory.
        Ok(())
    }

    fn read_dir(&self, path: &Path) -> Result<Vec<(String, bool)>> {
        let dir_error = |e: &io::Error| {
            Error::new(
                SqlState::IO_ERROR,
                format!("could not open directory \"{}\": {}", path.display(), os_text(e)),
            )
        };
        let mut out = Vec::new();
        for entry in fs::read_dir(path).map_err(|e| dir_error(&e))? {
            let entry = entry.map_err(|e| dir_error(&e))?;
            // A link to a directory counts as a directory, as `stat` sees it.
            let meta =
                fs::metadata(entry.path()).map_err(|e| os_error(&e, "stat", &entry.path()))?;
            out.push((entry.file_name().to_string_lossy().into_owned(), meta.is_dir()));
        }
        Ok(out)
    }

    #[cfg(unix)]
    fn mode(&self, path: &Path) -> Result<FileMode> {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(path).map_err(|e| {
            let state = if e.kind() == io::ErrorKind::NotFound {
                SqlState::UNDEFINED_FILE
            } else {
                SqlState::IO_ERROR
            };
            Error::new(state, os_text(&e))
        })?;
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        Ok(FileMode {
            regular: meta.is_file(),
            mine: meta.uid() == me,
            root: meta.uid() == 0,
            mode: meta.mode() & 0o7777,
        })
    }
}

/// A file of the operating system.
#[derive(Debug)]
pub struct OsFile {
    file: fs::File,
    path: PathBuf,
}

impl File for OsFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match read_exact_at(&self.file, buf, offset) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(Error::corrupted(format!(
                "could not read {} bytes at offset {offset} of file \"{}\": the file is shorter",
                buf.len(),
                self.path.display()
            ))),
            Err(e) => Err(os_error(&e, "read", &self.path)),
        }
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        write_all_at(&self.file, data, offset).map_err(|e| os_error(&e, "write to", &self.path))
    }

    fn sync(&self) -> Result<()> {
        // On macOS and iOS the standard library uses fcntl(F_FULLFSYNC), because fsync there does not flush the cache of the disk.
        self.file.sync_data().map_err(|e| os_error(&e, "fdatasync", &self.path))
    }

    fn size(&self) -> Result<u64> {
        Ok(self.file.metadata().map_err(|e| os_error(&e, "stat", &self.path))?.len())
    }

    fn set_size(&self, size: u64) -> Result<()> {
        self.file.set_len(size).map_err(|e| os_error(&e, "truncate", &self.path))
    }
}

#[cfg(unix)]
fn read_exact_at(file: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(unix)]
fn write_all_at(file: &fs::File, data: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, data, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &fs::File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &fs::File, mut data: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !data.is_empty() {
        match file.seek_write(data, offset) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                data = &data[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn read_exact_at(_: &fs::File, _: &mut [u8], _: u64) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(not(any(unix, windows)))]
fn write_all_at(_: &fs::File, _: &[u8], _: u64) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// The clock of the operating system.
#[derive(Clone, Copy, Debug)]
pub struct OsClock {
    start: Instant,
}

impl OsClock {
    /// A clock. Its monotonic time starts at zero now.
    pub fn new() -> OsClock {
        OsClock { start: Instant::now() }
    }
}

impl Default for OsClock {
    fn default() -> OsClock {
        OsClock::new()
    }
}

impl Clock for OsClock {
    fn wall_ms(&self) -> u64 {
        // A wall clock before 1970 is a broken clock. The HLC then counts on its logical part.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    fn monotonic(&self) -> Duration {
        self.start.elapsed()
    }
}

/// The random source of the operating system: `getentropy` on Unix.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsEntropy;

#[cfg(unix)]
impl Entropy for OsEntropy {
    fn fill(&self, buf: &mut [u8]) {
        // getentropy gives at most 256 bytes for each call.
        for chunk in buf.chunks_mut(256) {
            // SAFETY: the pointer and the length are those of a live mutable slice of at most 256 bytes.
            let r = unsafe { libc::getentropy(chunk.as_mut_ptr().cast(), chunk.len()) };
            assert!(r == 0, "getentropy failed: {}", io::Error::last_os_error());
        }
    }
}

#[cfg(not(unix))]
impl Entropy for OsEntropy {
    fn fill(&self, buf: &mut [u8]) {
        use std::hash::{BuildHasher, Hasher};
        // The standard library seeds each RandomState from the random source of the operating system.
        for chunk in buf.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(0);
            chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A new empty directory under the temporary directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("rupg-platform-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn whole_files_and_directories() {
        let dir = TempDir::new();
        let io = OsIo;
        fs::write(dir.0.join("a.conf"), "local all all trust\n").unwrap();
        fs::create_dir(dir.0.join("sub")).unwrap();
        assert_eq!(io.read_file(&dir.0.join("a.conf")).unwrap(), b"local all all trust\n");
        let mut entries = io.read_dir(&dir.0).unwrap();
        entries.sort();
        assert_eq!(entries, [("a.conf".to_string(), false), ("sub".to_string(), true)]);
        let missing = dir.0.join("none");
        let e = io.read_file(&missing).unwrap_err();
        assert_eq!(e.state(), SqlState::UNDEFINED_FILE);
        assert_eq!(
            e.message(),
            format!("could not open file \"{}\": No such file or directory", missing.display())
        );
        assert!(io.read_dir(&missing).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn owners_and_modes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let io = OsIo;
        let key = dir.0.join("server.key");
        fs::write(&key, "key").unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o640)).unwrap();
        let mode = io.mode(&key).unwrap();
        assert!(mode.regular && mode.mine);
        assert_eq!(mode.mode, 0o640);
        assert!(!io.mode(&dir.0).unwrap().regular);
        let e = io.mode(&dir.0.join("none")).unwrap_err();
        assert_eq!(e.state(), SqlState::UNDEFINED_FILE);
        assert_eq!(e.message(), "No such file or directory");
    }

    #[test]
    fn a_file_round_trip() {
        let dir = TempDir::new();
        let path = dir.0.join("a.rupg");
        let io = OsIo;
        assert!(!io.exists(&path).unwrap());
        let f = io.open(&path, OpenMode::CreateNew).unwrap();
        f.write_at(16384, b"page one").unwrap();
        f.sync().unwrap();
        io.sync_dir(&dir.0).unwrap();
        assert_eq!(f.size().unwrap(), 16392);
        let mut buf = [0xff; 8];
        f.read_at(16384, &mut buf).unwrap();
        assert_eq!(&buf, b"page one");
        // The bytes before the write are zero.
        f.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [0; 8]);
        f.set_size(32768).unwrap();
        assert_eq!(f.size().unwrap(), 32768);
        drop(f);

        let e = io.open(&path, OpenMode::CreateNew).unwrap_err();
        assert_eq!(e.state(), SqlState::DUPLICATE_FILE);
        let f = io.open(&path, OpenMode::Read).unwrap();
        assert!(f.write_at(0, b"x").is_err());
        let e = f.read_at(32760, &mut [0; 16]).unwrap_err();
        assert_eq!(e.state(), SqlState::DATA_CORRUPTED);
        drop(f);

        io.remove(&path).unwrap();
        let e = io.open(&path, OpenMode::ReadWrite).unwrap_err();
        assert_eq!(e.state(), SqlState::UNDEFINED_FILE);
        assert!(e.message().contains("a.rupg"), "{e}");
    }

    #[test]
    fn threads() {
        let h = OsTasks.spawn("rupg-test", Box::new(|| {})).unwrap();
        h.join().unwrap();
        let h = OsTasks.spawn("rupg-bad", Box::new(|| panic!("a test panic"))).unwrap();
        assert!(h.join().unwrap_err().message().contains("rupg-bad"));
        assert!(OsTasks.parallelism() >= 1);
    }

    #[test]
    fn tcp() {
        use std::io::{Read, Write};

        let listener = OsNet.listen("127.0.0.1:0").unwrap();
        let addr = listener.local_addr();
        let server = OsTasks
            .spawn(
                "rupg-echo",
                Box::new(move || {
                    let mut s = listener.accept().unwrap();
                    let mut buf = [0; 5];
                    s.read_exact(&mut buf).unwrap();
                    s.write_all(&buf).unwrap();
                    assert_eq!(s.read(&mut buf).unwrap(), 0);
                }),
            )
            .unwrap();
        let mut c = OsNet.connect(&addr).unwrap();
        assert_eq!(c.peer_addr(), addr);
        c.write_all(b"hello").unwrap();
        let mut buf = [0; 5];
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        c.shutdown().unwrap();
        server.join().unwrap();
        // Port 1 is privileged and has no listener on a test host.
        assert_eq!(
            OsNet.connect("127.0.0.1:1").unwrap_err().state(),
            SqlState::SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_sockets() {
        use std::io::{Read, Write};

        let dir = TempDir::new();
        let path = dir.0.join(".s.PGSQL.5999").display().to_string();
        let lock = format!("{path}.lock");
        let listener = OsNet.listen(&path).unwrap();
        assert_eq!(listener.local_addr(), path);
        let text = fs::read_to_string(&lock).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], std::process::id().to_string());
        assert_eq!(lines[3..], ["5999", dir.0.to_str().unwrap()]);
        let server = OsTasks
            .spawn(
                "rupg-echo",
                Box::new(move || {
                    let mut s = listener.accept().unwrap();
                    assert!(s.is_local());
                    assert_eq!(s.peer_addr(), "[local]");
                    assert!(!s.peer_user().unwrap().is_empty());
                    let mut buf = [0; 5];
                    s.read_exact(&mut buf).unwrap();
                    s.write_all(&buf).unwrap();
                }),
            )
            .unwrap();
        let mut c = OsNet.connect(&path).unwrap();
        c.write_all(b"hello").unwrap();
        let mut buf = [0; 5];
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        server.join().unwrap();
        // The listener dropped with the task, and took the socket and the lock file with it.
        assert!(!Path::new(&path).exists());
        assert!(!Path::new(&lock).exists());
        assert!(OsNet.connect(&path).is_err());

        // Process 1 is alive, so its lock file holds.
        fs::write(&lock, "1\n").unwrap();
        let e = OsNet.listen(&path).unwrap_err();
        assert_eq!(e.state(), SqlState::LOCK_FILE_EXISTS);
        assert_eq!(e.message(), format!("lock file \"{lock}\" already exists"));
        assert_eq!(
            e.hint(),
            Some(format!("Is another postmaster (PID 1) using socket file \"{path}\"?").as_str())
        );
        // No process has this ID, so the lock file is old and the server takes it.
        fs::write(&lock, "999999999\n").unwrap();
        fs::write(&path, "").unwrap();
        let listener = OsNet.listen(&path).unwrap();
        drop(listener);
        fs::write(&lock, "x\n").unwrap();
        let e = OsNet.listen(&path).unwrap_err();
        assert_eq!(e.message(), format!("bogus data in lock file \"{lock}\": \"x\""));
    }

    #[cfg(unix)]
    #[test]
    fn lookups() {
        let loopback: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let interfaces = OsNet.interfaces().unwrap();
        assert!(interfaces.iter().any(|(address, _)| *address == loopback), "{interfaces:?}");
        let name = OsNet.host_name(loopback).unwrap();
        assert!(!name.is_empty());
        assert!(OsNet.host_addresses("localhost").unwrap().iter().any(|a| a.is_loopback()));
        assert!(OsNet.host_addresses("no-such-host.invalid").is_err());
    }

    #[test]
    fn the_clock_and_the_entropy() {
        let clock = OsClock::new();
        // 2026-01-01 UTC. A host clock before this date is wrong.
        assert!(clock.wall_ms() > 1_767_225_600_000);
        let a = clock.monotonic();
        assert!(clock.monotonic() >= a);
        assert!(clock.hlc_wall() < clock.wall_ms());

        let mut a = [0; 300];
        let mut b = [0; 300];
        OsEntropy.fill(&mut a);
        OsEntropy.fill(&mut b);
        assert_ne!(a, b);
        assert_ne!(OsEntropy.next_u64(), OsEntropy.next_u64());
    }
}
