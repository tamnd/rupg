//! The `Net` trait.

use std::fmt;
use std::io::{Read, Write};
use std::net::IpAddr;

use rupg_common::{Error, Result, SqlState};

/// The network.
///
/// An address has the form `host:port`, or it is the path of a Unix socket, which starts with `/`.
pub trait Net: Send + Sync + fmt::Debug {
    /// Listens on an address. Port 0 picks a free port. [`Listener::local_addr`] gives it.
    ///
    /// On a Unix socket, the operating system implementation first creates the lock file of PostgreSQL, `<path>.lock`, so that a second server cannot take the socket. It removes the socket and the lock file when the listener drops.
    fn listen(&self, addr: &str) -> Result<Box<dyn Listener>>;

    /// Connects to an address. It is an error with SQLSTATE `08001` if no one listens there.
    fn connect(&self, addr: &str) -> Result<Box<dyn Stream>>;

    /// The address of each network interface of this machine, with its mask. `samehost` and `samenet` of `pg_hba.conf` use them. A network without interfaces gives none.
    ///
    /// # Errors
    ///
    /// The system cannot list them. The message is the text of the system.
    fn interfaces(&self) -> Result<Vec<(IpAddr, IpAddr)>> {
        Ok(Vec::new())
    }

    /// The host name of an address, from a reverse lookup that requires a name.
    ///
    /// # Errors
    ///
    /// The lookup fails. The message is the text of `gai_strerror`.
    fn host_name(&self, ip: IpAddr) -> Result<String> {
        let _ = ip;
        Err(Error::new(SqlState::CONNECTION_EXCEPTION, "Name or service not known"))
    }

    /// The addresses of a host name, from a forward lookup.
    ///
    /// # Errors
    ///
    /// The lookup fails. The message is the text of `gai_strerror`.
    fn host_addresses(&self, name: &str) -> Result<Vec<IpAddr>> {
        let _ = name;
        Err(Error::new(SqlState::CONNECTION_EXCEPTION, "Name or service not known"))
    }
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

    /// The address of the other side. It is `[local]` on a Unix socket, as in the log of PostgreSQL.
    fn peer_addr(&self) -> String;

    /// True when the connection is a Unix socket.
    fn is_local(&self) -> bool {
        false
    }

    /// The name of the user of the operating system that runs the other side, for the method `peer`. Only a Unix socket of the operating system knows it.
    ///
    /// # Errors
    ///
    /// The connection is not a Unix socket, or the system cannot give the user.
    fn peer_user(&self) -> Result<String> {
        Err(Error::internal("the connection is not a Unix socket"))
    }
}
