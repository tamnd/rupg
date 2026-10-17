//! TCP and Unix sockets of the operating system.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};
use std::time::Duration;

use rupg_common::{Error, Result, SqlState};

use crate::net::{Listener, Net, Stream, TcpOption};

/// The sockets of the operating system. `TCP_NODELAY` is on for each TCP connection, because the protocol sends small messages and waits for the answer. `SO_KEEPALIVE` is on for each TCP connection that a listener accepts, as `StreamConnection` of PostgreSQL sets it.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsNet;

impl Net for OsNet {
    fn listen(&self, addr: &str) -> Result<Box<dyn Listener>> {
        if addr.starts_with('/') {
            return unix_listen(addr);
        }
        let listener = TcpListener::bind(addr).map_err(|e| {
            Error::new(SqlState::IO_ERROR, format!("could not bind to the address \"{addr}\": {e}"))
        })?;
        Ok(Box::new(OsListener(listener)))
    }

    fn connect(&self, addr: &str) -> Result<Box<dyn Stream>> {
        if addr.starts_with('/') {
            return unix_connect(addr);
        }
        let stream = TcpStream::connect(addr).map_err(|e| {
            Error::new(
                SqlState::SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION,
                format!("could not connect to the address \"{addr}\": {e}"),
            )
        })?;
        OsStream::boxed(stream)
    }

    #[cfg(unix)]
    fn interfaces(&self) -> Result<Vec<(IpAddr, IpAddr)>> {
        super::lookup::interfaces()
    }

    #[cfg(unix)]
    fn host_name(&self, ip: IpAddr) -> Result<String> {
        super::lookup::host_name(ip)
    }

    #[cfg(unix)]
    fn host_addresses(&self, name: &str) -> Result<Vec<IpAddr>> {
        super::lookup::host_addresses(name)
    }
}

#[cfg(unix)]
use super::unix::{connect as unix_connect, listen as unix_listen};

#[cfg(not(unix))]
fn unix_listen(addr: &str) -> Result<Box<dyn Listener>> {
    Err(Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("could not bind to the address \"{addr}\": Unix sockets are not supported"),
    ))
}

#[cfg(not(unix))]
fn unix_connect(addr: &str) -> Result<Box<dyn Stream>> {
    Err(Error::new(
        SqlState::SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION,
        format!("could not connect to the address \"{addr}\": Unix sockets are not supported"),
    ))
}

#[derive(Debug)]
struct OsListener(TcpListener);

impl Listener for OsListener {
    fn accept(&self) -> Result<Box<dyn Stream>> {
        let (stream, _) = self.0.accept().map_err(|e| {
            Error::new(SqlState::CONNECTION_FAILURE, format!("could not accept a connection: {e}"))
        })?;
        keep_alive(&stream).map_err(|e| {
            Error::new(
                SqlState::CONNECTION_FAILURE,
                format!("setsockopt(SO_KEEPALIVE) failed: {e}"),
            )
        })?;
        OsStream::boxed(stream)
    }

    fn local_addr(&self) -> String {
        self.0.local_addr().map_or_else(|_| "unknown".to_string(), |a| a.to_string())
    }
}

#[derive(Debug)]
struct OsStream(TcpStream);

impl OsStream {
    fn boxed(stream: TcpStream) -> Result<Box<dyn Stream>> {
        stream.set_nodelay(true).map_err(|e| {
            Error::new(SqlState::CONNECTION_FAILURE, format!("could not set TCP_NODELAY: {e}"))
        })?;
        Ok(Box::new(OsStream(stream)))
    }
}

impl Read for OsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for OsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    /// One `writev` for all the parts. TLS gives each record of the handshake as one part, and the default sends one part at a time.
    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.0.write_vectored(bufs)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Stream for OsStream {
    fn shutdown(&self) -> Result<()> {
        match self.0.shutdown(Shutdown::Both) {
            // The other side closed first.
            Err(e) if e.kind() != io::ErrorKind::NotConnected => Err(Error::new(
                SqlState::CONNECTION_FAILURE,
                format!("could not close the connection: {e}"),
            )),
            _ => Ok(()),
        }
    }

    fn peer_addr(&self) -> String {
        self.0.peer_addr().map_or_else(|_| "unknown".to_string(), |a| a.to_string())
    }

    fn local_addr(&self) -> String {
        self.0.local_addr().map(|a| a.to_string()).unwrap_or_default()
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        self.0.set_read_timeout(timeout).map_err(|e| timeout_error(&e))
    }

    fn tcp_option(&self, option: TcpOption) -> Option<i32> {
        get_tcp_option(&self.0, option)
    }

    fn set_tcp_option(&self, option: TcpOption, value: i32) -> Result<()> {
        set_tcp_option(&self.0, option, value).map_err(|e| {
            Error::new(
                SqlState::CONNECTION_FAILURE,
                format!("setsockopt({}) failed: {e}", option.name()),
            )
        })
    }
}

/// The size of an option of the type `int` of C.
#[cfg(target_os = "linux")]
const INT_SIZE: libc::socklen_t = 4;

/// The name of the option for `getsockopt` and `setsockopt` at the level `IPPROTO_TCP`.
#[cfg(target_os = "linux")]
fn tcp_name(option: TcpOption) -> libc::c_int {
    match option {
        TcpOption::KeepalivesIdle => libc::TCP_KEEPIDLE,
        TcpOption::KeepalivesInterval => libc::TCP_KEEPINTVL,
        TcpOption::KeepalivesCount => libc::TCP_KEEPCNT,
        TcpOption::UserTimeout => libc::TCP_USER_TIMEOUT,
    }
}

/// `setsockopt` of an option of the type `int`.
#[cfg(target_os = "linux")]
fn set_option(
    socket: &TcpStream,
    level: libc::c_int,
    name: libc::c_int,
    value: i32,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the socket stays open while `socket` lives, and `value` is an `int` that lives for the call.
    let result = unsafe {
        libc::setsockopt(socket.as_raw_fd(), level, name, (&raw const value).cast(), INT_SIZE)
    };
    if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Turns on `SO_KEEPALIVE`.
#[cfg(target_os = "linux")]
fn keep_alive(socket: &TcpStream) -> io::Result<()> {
    set_option(socket, libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1)
}

/// The value of a TCP option of the socket.
#[cfg(target_os = "linux")]
fn get_tcp_option(socket: &TcpStream, option: TcpOption) -> Option<i32> {
    use std::os::fd::AsRawFd;
    let mut value: libc::c_int = 0;
    let mut size = INT_SIZE;
    // SAFETY: the socket stays open while `socket` lives, and `value` and `size` can take a write of their sizes.
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_TCP,
            tcp_name(option),
            (&raw mut value).cast(),
            &raw mut size,
        )
    };
    (result == 0).then_some(value)
}

/// Sets a TCP option of the socket.
#[cfg(target_os = "linux")]
fn set_tcp_option(socket: &TcpStream, option: TcpOption, value: i32) -> io::Result<()> {
    set_option(socket, libc::IPPROTO_TCP, tcp_name(option), value)
}

/// Only Linux is done, so the other systems leave `SO_KEEPALIVE` off.
#[cfg(not(target_os = "linux"))]
fn keep_alive(_socket: &TcpStream) -> io::Result<()> {
    Ok(())
}

/// Only Linux is done, so the other systems do not know the options.
#[cfg(not(target_os = "linux"))]
fn get_tcp_option(_socket: &TcpStream, _option: TcpOption) -> Option<i32> {
    None
}

/// Only Linux is done, so the other systems do not change the options.
#[cfg(not(target_os = "linux"))]
fn set_tcp_option(_socket: &TcpStream, _option: TcpOption, _value: i32) -> io::Result<()> {
    Ok(())
}

/// The error of `setsockopt` for `SO_RCVTIMEO`.
pub(super) fn timeout_error(e: &io::Error) -> Error {
    Error::new(SqlState::CONNECTION_FAILURE, format!("could not set the read timeout: {e}"))
}
