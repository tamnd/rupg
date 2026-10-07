//! TCP of the operating system.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};

use rupg_common::{Error, Result, SqlState};

use crate::net::{Listener, Net, Stream};

/// TCP sockets of the operating system. `TCP_NODELAY` is on for each connection, because the protocol sends small messages and waits for the answer.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsNet;

impl Net for OsNet {
    fn listen(&self, addr: &str) -> Result<Box<dyn Listener>> {
        let listener = TcpListener::bind(addr).map_err(|e| {
            Error::new(SqlState::IO_ERROR, format!("could not bind to the address \"{addr}\": {e}"))
        })?;
        Ok(Box::new(OsListener(listener)))
    }

    fn connect(&self, addr: &str) -> Result<Box<dyn Stream>> {
        let stream = TcpStream::connect(addr).map_err(|e| {
            Error::new(
                SqlState::UNABLE_TO_CONNECT,
                format!("could not connect to the address \"{addr}\": {e}"),
            )
        })?;
        OsStream::boxed(stream)
    }
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
}
