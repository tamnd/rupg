//! The `Net` trait.

use std::fmt;
use std::io::{Read, Write};

use rupg_common::Result;

/// The network.
pub trait Net: Send + Sync + fmt::Debug {
    /// Listens on an address of the form `host:port`. Port 0 picks a free port. [`Listener::local_addr`] gives it.
    fn listen(&self, addr: &str) -> Result<Box<dyn Listener>>;

    /// Connects to an address of the form `host:port`. It is an error with SQLSTATE `08001` if no one listens there.
    fn connect(&self, addr: &str) -> Result<Box<dyn Stream>>;
}

/// A socket that accepts connections.
pub trait Listener: Send + Sync + fmt::Debug {
    /// Waits for the next connection.
    fn accept(&self) -> Result<Box<dyn Stream>>;

    /// The address of the socket, with the port that the system picked.
    fn local_addr(&self) -> String;
}

/// A connection. A read gives 0 bytes when the other side has closed the connection.
pub trait Stream: Read + Write + Send + fmt::Debug {
    /// Closes both directions. The other side reads the end of the stream.
    fn shutdown(&self) -> Result<()>;

    /// The address of the other side.
    fn peer_addr(&self) -> String;
}
