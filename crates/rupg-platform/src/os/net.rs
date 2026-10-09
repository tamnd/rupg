//! TCP and Unix sockets of the operating system.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};

use rupg_common::{Error, Result, SqlState};

use crate::net::{Listener, Net, Stream};

/// The sockets of the operating system. `TCP_NODELAY` is on for each TCP connection, because the protocol sends small messages and waits for the answer.
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
}
