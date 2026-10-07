//! The PostgreSQL server, with the feature `server`.
//!
//! [`serve`] starts a server on the operating system. A client such as `psql` connects to its address. At this step the server runs `SET`, `SHOW`, `RESET`, `DISCARD` and the statements of a transaction block, and gives the error `0A000` for any other statement.

use std::path::Path;
use std::sync::Arc;

use rupg_common::{Error, Result, SqlState};
use rupg_platform::Io;
use rupg_platform::os::{OsEntropy, OsIo, OsNet, OsTasks};
pub use rupg_server::{Config, Log, Server};

/// Starts a server on the operating system with the settings of `config`.
///
/// # Errors
///
/// A setting that is not valid, host rules that do not load, and an address that the server cannot listen on.
pub fn serve(config: &Config) -> Result<Server> {
    Server::start(config, Arc::new(OsNet), Arc::new(OsIo), Arc::new(OsTasks), Arc::new(OsEntropy))
}

/// The password of the superuser from the first line of the file `path`, as `initdb --pwfile` reads it.
///
/// # Errors
///
/// A file that cannot be read, and a file with an empty first line.
pub fn read_password(path: &str) -> Result<String> {
    let bytes = OsIo.read_file(Path::new(path)).map_err(|e| {
        let reason = e.message().rsplit_once("\": ").map_or("", |(_, reason)| reason);
        Error::new(e.state(), format!("could not open file \"{path}\" for reading: {reason}"))
    })?;
    let text = String::from_utf8_lossy(&bytes);
    let line = text.split('\n').next().unwrap_or("").trim_end_matches('\r');
    if line.is_empty() {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("password file \"{path}\" is empty"),
        ));
    }
    Ok(line.to_owned())
}
