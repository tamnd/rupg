//! The PostgreSQL server, with the feature `server`.
//!
//! [`serve`] starts a server on the operating system. A client such as `psql` connects to its address. At this step the server runs `SET`, `SHOW`, `RESET`, `DISCARD` and the statements of a transaction block, and gives the error `0A000` for any other statement.

use std::sync::Arc;

use rupg_common::Result;
use rupg_platform::os::{OsEntropy, OsNet, OsTasks};
pub use rupg_server::{Config, Server};

/// Starts a server on the operating system with the settings of `config`.
///
/// # Errors
///
/// A setting that is not valid, and an address that the server cannot listen on.
pub fn serve(config: &Config) -> Result<Server> {
    Server::start(config, Arc::new(OsNet), Arc::new(OsTasks), Arc::new(OsEntropy))
}
