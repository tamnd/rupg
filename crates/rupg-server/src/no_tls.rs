//! The build without the feature `tls`. The server has no TLS, so it answers `N` to each `SSLRequest`, closes each connection that starts with direct TLS, and does not start when `ssl` is on, as a build of PostgreSQL without SSL support does.

use rupg_common::{Error, Result, SqlState};
use rupg_platform::{Io, Stream};
use rupg_session::guc::Settings;

use crate::Secured;

/// The TLS configuration, which this build never has.
#[derive(Debug)]
pub(crate) enum Tls {}

impl Tls {
    pub(crate) fn ca(&self) -> bool {
        match *self {}
    }
}

/// `check_ssl` of PostgreSQL without `USE_SSL`.
///
/// # Errors
///
/// `ssl` is on.
pub(crate) fn load(settings: &Settings, _: &dyn Io) -> Result<Option<Tls>> {
    if settings.get("ssl").as_deref() == Some("on") {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "SSL is not supported by this build",
        ));
    }
    Ok(None)
}

pub(crate) fn accept(
    tls: &Tls,
    _: &mut Box<dyn Stream>,
    _: &[u8],
    _: bool,
) -> std::result::Result<Secured, Option<String>> {
    match *tls {}
}
