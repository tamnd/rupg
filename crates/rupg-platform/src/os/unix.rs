//! Unix sockets of the operating system, with the lock file of PostgreSQL next to each socket.
//!
//! The lock file follows `CreateLockFile` in `miscinit.c`: the server creates `<socket>.lock` with `O_EXCL`. When the file is there, the server reads the process ID on its first line. If that process is alive, the server does not start. If it is gone, the server removes the old lock file and tries again.

use std::ffi::CStr;
use std::fs::{self, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rupg_common::{Error, Result, SqlState};

use super::net::timeout_error;
use super::os_text;
use crate::net::{Listener, Stream};

/// The number of times that the server tries to create a lock file, as in `CreateLockFile`.
const LOCK_TRIES: usize = 100;
/// The mode of the socket file, which is the default of `unix_socket_permissions`.
const SOCKET_MODE: u32 = 0o777;
/// The mode of the lock file.
const LOCK_MODE: u32 = 0o600;

/// Creates the lock file and listens on the socket `path`.
pub(super) fn listen(path: &str) -> Result<Box<dyn Listener>> {
    let lock = format!("{path}.lock");
    create_lock(path, &lock)?;
    // A socket file that no lock file covers belongs to no server, so PostgreSQL removes it before the bind.
    let _ = fs::remove_file(path);
    let bound = UnixListener::bind(path).and_then(|listener| {
        fs::set_permissions(path, Permissions::from_mode(SOCKET_MODE))?;
        Ok(listener)
    });
    match bound {
        Ok(listener) => Ok(Box::new(OsUnixListener { listener, path: path.to_string(), lock })),
        Err(e) => {
            let _ = fs::remove_file(path);
            let _ = fs::remove_file(&lock);
            Err(Error::new(
                SqlState::IO_ERROR,
                format!("could not bind to the address \"{path}\": {e}"),
            ))
        }
    }
}

/// Connects to the socket `path`.
pub(super) fn connect(path: &str) -> Result<Box<dyn Stream>> {
    let stream = UnixStream::connect(path).map_err(|e| {
        Error::new(
            SqlState::SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION,
            format!("could not connect to the address \"{path}\": {e}"),
        )
    })?;
    Ok(Box::new(OsUnixStream(stream)))
}

/// The contents of the lock file of a socket: the process ID, the data directory, the start time, the port and the directory of the socket, one on each line. The server has no data directory yet, so that line is empty.
fn lock_text(socket: &str) -> String {
    let (dir, name) = socket.rsplit_once('/').unwrap_or(("", socket));
    let dir = if dir.is_empty() { "/" } else { dir };
    let port = name.strip_prefix(".s.PGSQL.").and_then(|p| p.parse::<u16>().ok()).unwrap_or(0);
    let start = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    format!("{}\n\n{start}\n{port}\n{dir}\n", std::process::id())
}

/// True when the process `pid` exists. A process of another user, which the server cannot signal, exists too.
fn alive(pid: i32) -> bool {
    // SAFETY: the signal 0 only checks that the process exists and that the caller may signal it.
    let sent = unsafe { libc::kill(pid, 0) };
    sent == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// `CreateLockFile` for the socket `socket`.
fn create_lock(socket: &str, lock: &str) -> Result<()> {
    let file_error = |action: &str, e: &io::Error| {
        Error::new(SqlState::IO_ERROR, format!("could not {action} lock file \"{lock}\": {e}"))
    };
    let own = [std::process::id(), std::os::unix::process::parent_id()];
    for _ in 0..LOCK_TRIES {
        let created = OpenOptions::new().write(true).create_new(true).mode(LOCK_MODE).open(lock);
        let e = match created {
            Ok(mut file) => {
                return file
                    .write_all(lock_text(socket).as_bytes())
                    .and_then(|()| file.sync_all())
                    .map_err(|e| {
                        let _ = fs::remove_file(lock);
                        file_error("write", &e)
                    });
            }
            Err(e) => e,
        };
        if e.kind() != io::ErrorKind::AlreadyExists {
            return Err(file_error("create", &e));
        }
        let mut text = String::new();
        match fs::File::open(lock).and_then(|mut file| file.read_to_string(&mut text)) {
            Ok(_) => {}
            // The other server removed it in the meantime.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(file_error("open", &e)),
        }
        if text.is_empty() {
            return Err(Error::new(
                SqlState::LOCK_FILE_EXISTS,
                format!("lock file \"{lock}\" is empty"),
            )
            .with_hint("Either another server is starting, or the lock file is the remnant of a previous server startup crash."));
        }
        let first = text.lines().next().unwrap_or("");
        let Ok(encoded) = first.trim().parse::<i32>() else {
            return Err(Error::new(
                SqlState::LOCK_FILE_EXISTS,
                format!("bogus data in lock file \"{lock}\": \"{first}\""),
            ));
        };
        let pid = encoded.unsigned_abs();
        let pid_i32 = i32::try_from(pid).unwrap_or(i32::MAX);
        if !own.contains(&pid) && alive(pid_i32) {
            let who = if encoded < 0 { "postgres" } else { "postmaster" };
            return Err(Error::new(
                SqlState::LOCK_FILE_EXISTS,
                format!("lock file \"{lock}\" already exists"),
            )
            .with_hint(format!("Is another {who} (PID {pid}) using socket file \"{socket}\"?")));
        }
        if let Err(e) = fs::remove_file(lock)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(Error::new(
                SqlState::IO_ERROR,
                format!("could not remove old lock file \"{lock}\": {e}"),
            )
            .with_hint("The file seems accidentally left over, but it could not be removed. Please remove the file by hand and try again."));
        }
    }
    Err(Error::new(SqlState::IO_ERROR, format!("could not create lock file \"{lock}\"")))
}

#[derive(Debug)]
struct OsUnixListener {
    listener: UnixListener,
    path: String,
    lock: String,
}

impl Listener for OsUnixListener {
    fn accept(&self) -> Result<Box<dyn Stream>> {
        let (stream, _) = self.listener.accept().map_err(|e| {
            Error::new(SqlState::CONNECTION_FAILURE, format!("could not accept a connection: {e}"))
        })?;
        Ok(Box::new(OsUnixStream(stream)))
    }

    fn local_addr(&self) -> String {
        self.path.clone()
    }
}

impl Drop for OsUnixListener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(&self.lock);
    }
}

#[derive(Debug)]
struct OsUnixStream(UnixStream);

impl Read for OsUnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for OsUnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Stream for OsUnixStream {
    fn shutdown(&self) -> Result<()> {
        match self.0.shutdown(Shutdown::Both) {
            Err(e) if e.kind() != io::ErrorKind::NotConnected => Err(Error::new(
                SqlState::CONNECTION_FAILURE,
                format!("could not close the connection: {e}"),
            )),
            _ => Ok(()),
        }
    }

    fn peer_addr(&self) -> String {
        "[local]".to_string()
    }

    fn is_local(&self) -> bool {
        true
    }

    fn peer_user(&self) -> Result<String> {
        user_name(peer_uid(&self.0)?)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        self.0.set_read_timeout(timeout).map_err(|e| timeout_error(&e))
    }
}

/// The user ID of the process on the other side of the socket, as `getpeereid` gives it.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid(stream: &UnixStream) -> Result<libc::uid_t> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = libc::socklen_t::try_from(size_of::<libc::ucred>()).unwrap_or(0);
    // SAFETY: `cred` and `len` are valid for writes, and `len` holds the size of `cred`.
    let got = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    if got != 0 {
        return Err(credentials_error());
    }
    Ok(cred.uid)
}

/// The user ID of the process on the other side of the socket, as `getpeereid` gives it.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn peer_uid(stream: &UnixStream) -> Result<libc::uid_t> {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: `uid` and `gid` are valid for writes.
    let got = unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) };
    if got != 0 {
        return Err(credentials_error());
    }
    Ok(uid)
}

fn credentials_error() -> Error {
    Error::new(
        SqlState::IO_ERROR,
        format!("could not get peer credentials: {}", os_text(&io::Error::last_os_error())),
    )
}

/// The name of the user `uid`, with the messages of `auth_peer` in `auth.c`.
fn user_name(uid: libc::uid_t) -> Result<String> {
    const BUFFER: usize = 16 * 1024;
    // SAFETY: `passwd` is plain data, and all zeros is a valid value of it.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; BUFFER];
    let mut found = std::ptr::null_mut();
    // SAFETY: each pointer is valid for writes, and `buffer` has the length that the call gets.
    let code = unsafe {
        libc::getpwuid_r(uid, &raw mut entry, buffer.as_mut_ptr(), buffer.len(), &raw mut found)
    };
    if found.is_null() {
        let message = if code == 0 {
            format!("local user with ID {uid} does not exist")
        } else {
            format!(
                "could not look up local user ID {uid}: {}",
                os_text(&io::Error::from_raw_os_error(code))
            )
        };
        return Err(Error::new(SqlState::INTERNAL_ERROR, message));
    }
    // SAFETY: on success `pw_name` points to a string that ends with a zero byte inside `buffer`.
    let name = unsafe { CStr::from_ptr(entry.pw_name) };
    Ok(name.to_string_lossy().into_owned())
}
