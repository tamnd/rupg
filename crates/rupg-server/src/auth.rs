//! Client authentication: the line of `pg_hba.conf` that matches the connection, and the exchange of its method, as `ClientAuthentication` of PostgreSQL runs them (spec/06 sections 6.7 and 6.8).
//!
//! A failure sends the `FATAL` error of PostgreSQL to the client and writes the same error to the log, with the reason and the line of `pg_hba.conf` in its `DETAIL`. The client never sees the reason, so it cannot tell a bad password from a role that does not exist.
//!
//! A TLS connection offers SCRAM with channel binding, and `cert` and `clientcert=verify-full` compare the common name or the distinguished name of the client certificate with the user, through the map of the line. `ident` on TCP, which asks the Ident server of the client (RFC 1413), is not done yet and fails with a line in the log.
//!
//! Lifted from `crates/rudb-server/src/session/auth.rs` of tamnd/rudb at f5f7065a.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use rupg_common::{Error, SqlState};
use rupg_session::connection::Start;
use rupg_wire::{
    Exchange, Hashes, Level, MD5_WARNING, MD5_WARNING_DETAIL, Mode, PasswordType, ProtocolError,
    SCRAM_NONCE_LEN, Scram, password_message, split, verify_md5, verify_password,
};

use crate::hba::{Client, ClientCert, ClientName, HbaLine, Method};
use crate::{READ_SIZE, Shared, Wire};

/// A warning that goes to the client at the end of the startup, `StoreConnectionWarning`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Notice {
    pub(crate) message: String,
    pub(crate) detail: String,
    /// The warning about an MD5 secret, which the setting `md5_password_warnings` turns off.
    pub(crate) md5: bool,
}

impl Notice {
    fn md5() -> Notice {
        Notice { message: MD5_WARNING.to_owned(), detail: MD5_WARNING_DETAIL.to_owned(), md5: true }
    }
}

/// What an authentication step leads to.
enum Step {
    Ok,
    /// The check failed, with the reason for the log.
    Failed(Option<String>),
    /// The connection ended: the client closed it, or an error already went out.
    Ended,
}

/// Runs the authentication of a new connection. `Some` holds the warnings for the end of the startup, and then the caller sends `AuthenticationOk`. `None` means that the connection ends, and the error, if any, already went out.
pub(crate) fn authenticate(
    shared: &Shared,
    wire: &mut Wire,
    start: &Start,
    protocol: u32,
) -> Option<Vec<Notice>> {
    let address = if wire.stream.is_local() {
        None
    } else {
        let peer = wire.stream.peer_addr();
        let ip = peer.parse::<SocketAddr>().map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |a| a.ip());
        Some(ip.to_canonical())
    };
    let ssl = wire.secured.is_some();
    let client = Client::new(address, ssl, &start.user, &start.database);
    let encryption = if ssl { "SSL encryption" } else { "no encryption" };
    let mut lines = Vec::new();
    let found = shared.hba.find(&client, &shared.roles, &*shared.net, &mut lines);
    shared.log_all("LOG", &lines);
    let Some(line) = found else {
        let message = format!(
            "no pg_hba.conf entry for host \"{}\", user \"{}\", database \"{}\", {encryption}",
            client.host(),
            start.user,
            start.database
        );
        let error = Error::new(SqlState::INVALID_AUTHORIZATION_SPECIFICATION, message);
        fatal(shared, wire, &error, client.lookup_detail());
        return None;
    };
    if line.clientcert != ClientCert::Off {
        if !shared.tls.as_ref().is_some_and(|tls| tls.ca()) {
            let message =
                "client certificates can only be checked if a root certificate store is available";
            fatal(shared, wire, &Error::new(SqlState::CONFIG_FILE_ERROR, message), None);
            return None;
        }
        if !wire.secured.as_ref().is_some_and(|secured| secured.peer.is_some()) {
            let message = "connection requires a valid client certificate";
            let error = Error::new(SqlState::INVALID_AUTHORIZATION_SPECIFICATION, message);
            fatal(shared, wire, &error, None);
            return None;
        }
    }
    if line.method == Method::Reject {
        let message = format!(
            "pg_hba.conf rejects connection for host \"{}\", user \"{}\", database \"{}\", {encryption}",
            client.host(),
            start.user,
            start.database
        );
        let error = Error::new(SqlState::INVALID_AUTHORIZATION_SPECIFICATION, message);
        fatal(shared, wire, &error, None);
        return None;
    }
    let mut notices = Vec::new();
    let mut run = Run { shared, start, protocol, wire };
    let step = match line.method {
        // `cert` is `trust` with the check of the certificate below.
        Method::Trust | Method::Cert => Step::Ok,
        Method::Peer => run.peer(line),
        Method::Password => run.password(&mut notices),
        Method::Md5 | Method::Scram => run.challenge(line.method, &mut notices),
        Method::Ident | Method::OAuth => {
            shared.log(
                "LOG",
                &format!(
                    "authentication method \"{}\" is not supported for this connection by this version of rupg",
                    line.method.name()
                ),
            );
            Step::Failed(None)
        }
        Method::Reject => Step::Failed(None),
    };
    let step = match step {
        Step::Ok if line.clientcert == ClientCert::VerifyFull || line.method == Method::Cert => {
            run.cert(line)
        }
        step => step,
    };
    match step {
        Step::Ok => Some(notices),
        Step::Ended => None,
        Step::Failed(detail) => {
            failed(shared, run.wire, line, &start.user, detail);
            None
        }
    }
}

/// Sends a `FATAL` error without its detail, and writes it to the log with the detail. `errdetail_log` of PostgreSQL does the same.
fn fatal(shared: &Shared, wire: &mut Wire, error: &Error, detail: Option<String>) {
    match detail {
        Some(detail) => shared.log("FATAL", &format!("{}\nDETAIL:  {detail}", error.message())),
        None => shared.log("FATAL", error.message()),
    }
    wire.fatal(error);
}

/// `auth_failed`: the error of the method to the client, and to the log with the reason and the line of `pg_hba.conf`.
fn failed(shared: &Shared, wire: &mut Wire, line: &HbaLine, user: &str, detail: Option<String>) {
    let (state, message) = line.method.failed(user);
    let matched =
        format!("Connection matched file \"{}\" line {}: \"{}\"", line.file, line.number, line.raw);
    let detail = match detail {
        Some(detail) => format!("{detail}\n{matched}"),
        None => matched,
    };
    fatal(shared, wire, &Error::new(state, message), Some(detail));
}

/// The state of one authentication.
struct Run<'a> {
    shared: &'a Shared,
    start: &'a Start,
    protocol: u32,
    wire: &'a mut Wire,
}

impl Run<'_> {
    /// Sends the output and reads one message of the exchange. `None` means that the connection ended.
    fn read(&mut self, mode: Mode) -> Option<Vec<u8>> {
        if !self.wire.flush() {
            return None;
        }
        loop {
            match split(self.wire.pending(), mode) {
                Ok(Some(frame)) => {
                    let body = frame.body.to_vec();
                    let size = frame.size();
                    self.wire.consume(size);
                    return Some(body);
                }
                // PostgreSQL ends the connection without a message when the client closes it here.
                Ok(None) => {
                    if !self.wire.fill(READ_SIZE) {
                        return None;
                    }
                }
                Err(error) => {
                    self.error(&error);
                    return None;
                }
            }
        }
    }

    /// Sends an error of the protocol, or writes it to the log when it is for the log only.
    fn error(&mut self, error: &ProtocolError) {
        if error.level == Level::Log {
            self.shared.log("LOG", &error.message);
            return;
        }
        match &error.detail {
            Some(detail) => {
                self.shared.log("FATAL", &format!("{}\nDETAIL:  {detail}", error.message));
            }
            None => self.shared.log("FATAL", &error.message),
        }
        self.wire.out.protocol_error(error, self.protocol);
        self.wire.flush();
    }

    /// Reads the password of a `PasswordMessage`. `None` means that the connection ended.
    fn password_message(&mut self) -> Option<Vec<u8>> {
        let body = self.read(Mode::Password)?;
        match password_message(&body) {
            Ok(password) => Some(password.to_vec()),
            Err(error) => {
                self.error(&error);
                None
            }
        }
    }

    /// The stored secret of the role, or the reason for the log when there is none.
    fn secret(&self) -> Result<&str, String> {
        let user = &self.start.user;
        if *user != self.shared.roles.superuser {
            return Err(format!("Role \"{user}\" does not exist."));
        }
        self.shared
            .secret
            .as_deref()
            .ok_or_else(|| format!("User \"{user}\" has no password assigned."))
    }

    /// `CheckPasswordAuth`: the `password` method, with the password in clear text.
    fn password(&mut self, notices: &mut Vec<Notice>) -> Step {
        self.wire.out.authentication_cleartext_password();
        let Some(password) = self.password_message() else {
            return Step::Ended;
        };
        let user = self.start.user.as_str();
        let secret = match self.secret() {
            Ok(secret) => secret,
            Err(detail) => return Step::Failed(Some(detail)),
        };
        if verify_password(&Hashes, user.as_bytes(), secret.as_bytes(), &password) {
            if PasswordType::of(secret.as_bytes()) == PasswordType::Md5 {
                notices.push(Notice::md5());
            }
            return Step::Ok;
        }
        Step::Failed(Some(match PasswordType::of(secret.as_bytes()) {
            PasswordType::Plaintext => {
                format!("Password of user \"{user}\" is in unrecognized format.")
            }
            _ => format!("Password does not match for user \"{user}\"."),
        }))
    }

    /// `CheckPWChallengeAuth`: `md5` with an MD5 secret, and SCRAM in every other case. A role that does not exist or has no valid secret goes through SCRAM with a mock secret, so the client cannot tell.
    fn challenge(&mut self, method: Method, notices: &mut Vec<Notice>) -> Step {
        let shared = self.shared;
        let user = self.start.user.clone();
        let (secret, mut detail) = match self.secret() {
            Ok(secret) => (Some(secret.to_owned()), None),
            Err(detail) => (None, Some(detail)),
        };
        // A role with no secret looks like a role with a secret of `password_encryption`.
        let kind =
            secret.as_deref().map_or(PasswordType::ScramSha256, |s| PasswordType::of(s.as_bytes()));
        if method == Method::Md5 && kind == PasswordType::Md5 {
            let secret = secret.unwrap_or_default();
            let mut salt = [0; 4];
            shared.entropy.fill(&mut salt);
            self.wire.out.authentication_md5_password(salt);
            let Some(response) = self.password_message() else {
                return Step::Ended;
            };
            if verify_md5(&Hashes, secret.as_bytes(), &response, salt) {
                notices.push(Notice::md5());
                return Step::Ok;
            }
            return Step::Failed(Some(format!("Password does not match for user \"{user}\".")));
        }
        if secret.is_some() && kind != PasswordType::ScramSha256 {
            detail = Some(format!("User \"{user}\" does not have a valid SCRAM secret."));
        }
        let hash = self.wire.secured.as_ref().map(|secured| secured.hash.clone());
        self.wire.out.authentication_sasl(Scram::mechanisms(hash.is_some()));
        let Some(body) = self.read(Mode::Sasl) else {
            return Step::Ended;
        };
        let secret = Scram::secret_for(
            &Hashes,
            user.as_bytes(),
            secret.as_deref().map(str::as_bytes),
            &shared.mock_nonce,
            shared.iterations,
        );
        let mut nonce = [0; SCRAM_NONCE_LEN];
        shared.entropy.fill(&mut nonce);
        let (mut scram, mut exchange) = match Scram::start(
            &Hashes,
            &body,
            hash.as_deref(),
            secret,
            nonce,
            &mut self.wire.out,
        ) {
            Ok(started) => started,
            Err(error) => {
                self.error(&error);
                return Step::Ended;
            }
        };
        loop {
            match exchange {
                // `AuthenticationSASLFinal` goes out with `AuthenticationOk`.
                Exchange::Success => return Step::Ok,
                Exchange::Failure => return Step::Failed(detail),
                Exchange::Continue => {}
            }
            let Some(body) = self.read(Mode::Sasl) else {
                return Step::Ended;
            };
            exchange = match scram.next(&Hashes, &body, &mut self.wire.out) {
                Ok(exchange) => exchange,
                Err(error) => {
                    self.error(&error);
                    return Step::Ended;
                }
            };
        }
    }

    /// `CheckCertAuth`: the common name or the distinguished name of the client certificate, through the map of the line.
    fn cert(&mut self, line: &HbaLine) -> Step {
        let user = &self.start.user;
        let peer = self.wire.secured.as_ref().and_then(|secured| secured.peer.as_ref());
        let name = peer.and_then(|peer| match line.clientname {
            ClientName::Cn => peer.cn.as_deref(),
            ClientName::Dn => Some(peer.dn.as_str()),
        });
        let Some(name) = name.filter(|name| !name.is_empty()) else {
            self.shared.log("LOG", &format!("certificate authentication failed for user \"{user}\": client certificate contains no user name"));
            return Step::Failed(None);
        };
        let mut lines = Vec::new();
        let ok = self.shared.ident.check(
            line.map.as_deref(),
            user,
            name,
            &self.shared.roles,
            &mut lines,
        );
        self.shared.log_all("LOG", &lines);
        if ok {
            return Step::Ok;
        }
        if line.clientcert == ClientCert::VerifyFull && line.method != Method::Cert {
            let field = match line.clientname {
                ClientName::Cn => "CN",
                ClientName::Dn => "DN",
            };
            self.shared.log("LOG", &format!("certificate validation (clientcert=verify-full) failed for user \"{user}\": {field} mismatch"));
        }
        Step::Failed(None)
    }

    /// `auth_peer`: the user that owns the other end of the Unix socket, through the map of the line.
    fn peer(&mut self, line: &HbaLine) -> Step {
        let name = match self.wire.stream.peer_user() {
            Ok(name) => name,
            Err(error) => {
                self.shared.log("LOG", error.message());
                return Step::Failed(None);
            }
        };
        let mut lines = Vec::new();
        let ok = self.shared.ident.check(
            line.map.as_deref(),
            &self.start.user,
            &name,
            &self.shared.roles,
            &mut lines,
        );
        self.shared.log_all("LOG", &lines);
        if ok { Step::Ok } else { Step::Failed(None) }
    }
}
