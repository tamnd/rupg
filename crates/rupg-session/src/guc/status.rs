//! The status of each parameter in rupg: honored, accepted or refused (spec/06 section 6.13.2).
//!
//! The lists are set by hand. A parameter that is in neither list is accepted: rupg stores and returns its value, and the value has no effect. A change that makes rupg read a parameter moves it to [`HONORED`] in the same change. The honored set grows with the milestones, so the lists say what rupg does now and not what a milestone will do.

use super::Parameter;

/// What rupg does with the value of a parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// rupg reads the value and behaves as PostgreSQL does.
    Honored,
    /// rupg stores and returns the value, and the value has no effect.
    Accepted,
    /// rupg refuses some values, because silent acceptance would change the meaning of data. The other values are accepted.
    Refused,
}

impl Status {
    /// The name of the status in the report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Honored => "honored",
            Self::Accepted => "accepted",
            Self::Refused => "refused",
        }
    }
}

/// The honored parameters with the part of rupg that reads each one, in the order of the names without regard to case.
pub static HONORED: [(&str, &str); 29] = [
    ("application_name", "The server reports it to the client."),
    (
        "client_encoding",
        "The session checks the strings of the messages. Only UTF8 and SQL_ASCII are taken.",
    ),
    ("client_min_messages", "The session sends only the notices at this level or above."),
    ("data_checksums", "rupg checks each page and has no way to turn it off."),
    ("hba_file", "The server reads the rules of client authentication from it."),
    ("ident_file", "The server reads the user name maps from it."),
    ("in_hot_standby", "rupg has no standby yet, so the value is off."),
    ("integer_datetimes", "rupg keeps the date and time types as integers."),
    ("is_superuser", "The session sets it from the role of the connection."),
    ("listen_addresses", "The server listens on these TCP addresses."),
    ("max_identifier_length", "The lexer cuts a longer name to 63 bytes."),
    ("md5_password_warnings", "The server warns when a client uses an MD5 password."),
    ("password_encryption", "The server stores new passwords with this method."),
    ("port", "The server listens on this port and names its socket with it."),
    ("scram_iterations", "The server uses this iteration count for new SCRAM secrets."),
    ("server_encoding", "rupg keeps all text in UTF8."),
    ("server_version", "The session reports the version of PostgreSQL and of rupg."),
    ("server_version_num", "The session reports the version of PostgreSQL."),
    ("session_authorization", "The session sets it from the role of the connection."),
    ("ssl", "The server takes TLS connections when it is on."),
    ("ssl_ca_file", "The server loads the root certificates for client certificates from it."),
    ("ssl_cert_file", "The server loads its certificate from it."),
    ("ssl_crl_file", "The server loads the revocation list from it."),
    ("ssl_key_file", "The server loads its private key from it."),
    ("ssl_max_protocol_version", "The server takes no TLS version above this one."),
    (
        "ssl_min_protocol_version",
        "The server takes no TLS version below this one, and none below TLSv1.2.",
    ),
    (
        "ssl_prefer_server_ciphers",
        "The server uses its own order of the cipher suites when it is on.",
    ),
    (
        "standard_conforming_strings",
        "PostgreSQL 19 takes only on, and the lexer reads strings with the standard rules.",
    ),
    ("unix_socket_directories", "The server makes its Unix sockets in these directories."),
];

/// The refused parameters, with the values that rupg refuses and the reason, in the order of the names without regard to case.
pub static REFUSED: [(&str, &[&str], &str); 4] = [
    (
        "default_transaction_isolation",
        &["serializable"],
        "rupg has no serializable isolation before M5, and a client must not get a weaker level without an error.",
    ),
    (
        "session_replication_role",
        &["replica"],
        "rupg has no triggers before M7, so it cannot keep the triggers of a replica from firing.",
    ),
    (
        "ssl_sni",
        &["on"],
        "rupg does not pick a certificate by the host name yet, and a client must not get another certificate than the one it asked for without an error.",
    ),
    (
        "transaction_isolation",
        &["serializable"],
        "rupg has no serializable isolation before M5, and a client must not get a weaker level without an error.",
    ),
];

/// The status of a parameter.
#[must_use]
pub fn status(parameter: &Parameter) -> Status {
    if HONORED.iter().any(|(name, _)| *name == parameter.name) {
        Status::Honored
    } else if REFUSED.iter().any(|(name, _, _)| *name == parameter.name) {
        Status::Refused
    } else {
        Status::Accepted
    }
}

/// True when rupg refuses `value`, the text that `SHOW` gives for a value of the parameter.
#[must_use]
pub fn refuses(parameter: &Parameter, value: &str) -> bool {
    REFUSED.iter().any(|(name, values, _)| *name == parameter.name && values.contains(&value))
}
