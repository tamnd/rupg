//! The password methods: the password message, `md5`, the check of a clear text password, and the server side of SCRAM-SHA-256 with and without channel binding.
//!
//! The code follows `auth.c`, `crypt.c` and `auth-scram.c` of PostgreSQL. The order of the checks is the order of PostgreSQL, so a client that sends bad bytes gets the same error from rupg. All the errors are [`Level::Fatal`]: PostgreSQL raises them at the level `ERROR`, but an error before the session exists ends the connection.
//!
//! The codec does not look up roles and does not make random bytes. The server gives the stored secret of the role, or `None` if the role does not exist or has no valid password, and the random bytes for the salt and the nonce.
//!
//! Lifted from `crates/rudb-pgwire/src/auth.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::fmt;

use crate::backend::OutBuf;
use crate::base64;
use crate::crypto::Crypto;
use crate::error::{Level, ProtocolError};
use crate::reader::Reader;

pub const SCRAM_SHA_256: &[u8] = b"SCRAM-SHA-256";
pub const SCRAM_SHA_256_PLUS: &[u8] = b"SCRAM-SHA-256-PLUS";
/// The number of random bytes in the nonce of the server, `SCRAM_RAW_NONCE_LEN`.
pub const SCRAM_NONCE_LEN: usize = 18;
/// The number of bytes in a new salt, `SCRAM_DEFAULT_SALT_LEN`.
pub const SCRAM_SALT_LEN: usize = 16;
/// The default of the setting `scram_iterations`.
pub const SCRAM_ITERATIONS: i32 = 4096;
/// The number of bytes in the mock nonce of the cluster, `MOCK_AUTH_NONCE_LEN`.
pub const MOCK_NONCE_LEN: usize = 32;

/// The warning that PostgreSQL sends after a login with an MD5 secret, while the setting `md5_password_warnings` is on.
pub const MD5_WARNING: &str = "authenticated with an MD5-encrypted password";
pub const MD5_WARNING_DETAIL: &str =
    "MD5 password support is deprecated and will be removed in a future release of PostgreSQL.";

const INVALID_AUTHORIZATION: &str = "28000";
const INVALID_PASSWORD: &str = "28P01";
const FEATURE_NOT_SUPPORTED: &str = "0A000";
const INTERNAL_ERROR: &str = "XX000";

/// The kind of a stored password, as `get_password_type` finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordType {
    Plaintext,
    Md5,
    ScramSha256,
}

impl PasswordType {
    pub fn of(secret: &[u8]) -> PasswordType {
        if secret.len() == 35
            && secret.starts_with(b"md5")
            && secret[3..].iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            PasswordType::Md5
        } else if ScramSecret::parse(secret).is_some() {
            PasswordType::ScramSha256
        } else {
            PasswordType::Plaintext
        }
    }
}

/// The error after a failed password check, from `auth_failed`. The text is the same for a bad password, a role that does not exist and a role with no password, so that a client cannot tell them apart.
pub fn password_failed(user: &[u8]) -> ProtocolError {
    ProtocolError::new(
        Level::Fatal,
        INVALID_PASSWORD,
        format!("password authentication failed for user \"{}\"", String::from_utf8_lossy(user)),
    )
}

/// The password in a `PasswordMessage`, without its zero byte, from `recv_password_packet`.
///
/// The body must be one string and nothing more, and the string must not be empty. The server does not convert the password from the client encoding, as PostgreSQL does not know that encoding yet either.
pub fn password_message(body: &[u8]) -> Result<&[u8], ProtocolError> {
    match body.iter().position(|&b| b == 0) {
        Some(0) if body.len() == 1 => Err(ProtocolError::new(
            Level::Fatal,
            INVALID_PASSWORD,
            "empty password returned by client",
        )),
        Some(end) if end + 1 == body.len() => Ok(&body[..end]),
        _ => Err(ProtocolError::fatal("invalid password packet size")),
    }
}

/// `pg_md5_encrypt`: `md5` and the hex digits of the MD5 of the password and the salt. A stored MD5 secret is this with the role name as the salt.
pub fn md5_encrypt(crypto: &(impl Crypto + ?Sized), password: &[u8], salt: &[u8]) -> String {
    let mut out = String::with_capacity(35);
    out.push_str("md5");
    for byte in crypto.md5(&[password, salt]) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `md5_crypt_verify`: the answer of the client to `AuthenticationMD5Password` with `salt`. Only an MD5 secret can pass. The client cannot make the answer from a SCRAM secret.
pub fn verify_md5(
    crypto: &(impl Crypto + ?Sized),
    secret: &[u8],
    response: &[u8],
    salt: [u8; 4],
) -> bool {
    PasswordType::of(secret) == PasswordType::Md5
        && same(md5_encrypt(crypto, &secret[3..], &salt).as_bytes(), response)
}

/// `plain_crypt_verify`: the clear text password of the `password` method against the stored secret. A secret in clear text never passes, as in PostgreSQL.
///
/// PostgreSQL applies SASLprep to the password before it checks a SCRAM secret. This function does not. SASLprep changes only a password that is valid UTF-8 with characters outside ASCII, so only such a password can pass in PostgreSQL and fail here.
pub fn verify_password(
    crypto: &(impl Crypto + ?Sized),
    user: &[u8],
    secret: &[u8],
    password: &[u8],
) -> bool {
    match PasswordType::of(secret) {
        PasswordType::Md5 => same(md5_encrypt(crypto, password, user).as_bytes(), secret),
        PasswordType::ScramSha256 => {
            let Some(stored) = ScramSecret::parse(secret) else { return false };
            let Some(salt) = base64::decode(stored.salt.as_bytes()) else { return false };
            let built = ScramSecret::build(crypto, password, &salt, stored.iterations);
            same(&built.server_key, &stored.server_key)
        }
        PasswordType::Plaintext => false,
    }
}

/// A SCRAM-SHA-256 secret as `pg_authid.rolpassword` stores it: `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScramSecret {
    pub iterations: i32,
    /// The salt in base64, as it was stored. The server sends it to the client in this form.
    pub salt: String,
    pub stored_key: [u8; 32],
    pub server_key: [u8; 32],
}

impl ScramSecret {
    /// `parse_scram_secret`. Like PostgreSQL, it takes an empty iteration count as zero and does not check that the count is above zero.
    pub fn parse(secret: &[u8]) -> Option<ScramSecret> {
        let (scheme, rest) = split_once(secret, b'$')?;
        let (iterations, rest) = split_once(rest, b':')?;
        let (salt, rest) = split_once(rest, b'$')?;
        let (stored_key, server_key) = split_once(rest, b':')?;
        if scheme != b"SCRAM-SHA-256" {
            return None;
        }
        let iterations = strtol(iterations)?;
        base64::decode(salt)?;
        Some(ScramSecret {
            iterations,
            salt: String::from_utf8(salt.to_vec()).ok()?,
            stored_key: base64::decode(stored_key)?.try_into().ok()?,
            server_key: base64::decode(server_key)?.try_into().ok()?,
        })
    }

    /// `scram_build_secret`, for a password that SASLprep has already prepared.
    pub fn build(
        crypto: &(impl Crypto + ?Sized),
        password: &[u8],
        salt: &[u8],
        iterations: i32,
    ) -> ScramSecret {
        let iterations_used = u32::try_from(iterations).unwrap_or(1).max(1);
        let salted = crypto.pbkdf2_sha256(password, salt, iterations_used);
        let client_key = crypto.hmac_sha256(&salted, &[b"Client Key"]);
        ScramSecret {
            iterations,
            salt: base64::encode(salt),
            stored_key: crypto.sha256(&[&client_key]),
            server_key: crypto.hmac_sha256(&salted, &[b"Server Key"]),
        }
    }

    /// `mock_scram_secret`, for a role that does not exist or has no SCRAM secret. The salt comes from the role name and the mock nonce of the cluster, so the same name always gets the same salt and the client cannot tell that the role is not there.
    pub fn mock(
        crypto: &(impl Crypto + ?Sized),
        user: &[u8],
        mock_nonce: &[u8; MOCK_NONCE_LEN],
        iterations: i32,
    ) -> ScramSecret {
        let digest = crypto.sha256(&[user, mock_nonce]);
        ScramSecret {
            iterations,
            salt: base64::encode(&digest[..SCRAM_SALT_LEN]),
            stored_key: [0; 32],
            server_key: [0; 32],
        }
    }
}

impl fmt::Display for ScramSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SCRAM-SHA-256${}:{}${}:{}",
            self.iterations,
            self.salt,
            base64::encode(&self.stored_key),
            base64::encode(&self.server_key)
        )
    }
}

/// What the server does after one step of SCRAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exchange {
    /// The step wrote `AuthenticationSASLContinue`. The server reads the next `SASLResponse`.
    Continue,
    /// The step wrote `AuthenticationSASLFinal`. The server sends `AuthenticationOk` next.
    Success,
    /// The proof is wrong, or the role has no valid secret. The step wrote nothing, and the server sends [`password_failed`].
    Failure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Init,
    SaltSent,
    Finished,
}

/// The server side of one SCRAM-SHA-256 exchange, `scram_state` in PostgreSQL.
#[derive(Debug, Clone)]
pub struct Scram {
    stage: Stage,
    /// The hash of the server certificate for `tls-server-end-point`, if the connection uses TLS.
    certificate_hash: Option<Vec<u8>>,
    /// The client selected `SCRAM-SHA-256-PLUS`.
    plus: bool,
    secret: ScramSecret,
    doomed: bool,
    nonce: [u8; SCRAM_NONCE_LEN],
    flag: u8,
    client_first_bare: Vec<u8>,
    client_nonce: Vec<u8>,
    server_nonce: String,
    server_first: Vec<u8>,
    client_key: [u8; 32],
}

impl Scram {
    /// The mechanisms for `AuthenticationSASL`, in the order of PostgreSQL. The variant with channel binding is only offered on TLS.
    pub fn mechanisms(tls: bool) -> &'static [&'static [u8]] {
        if tls { &[SCRAM_SHA_256_PLUS, SCRAM_SHA_256] } else { &[SCRAM_SHA_256] }
    }

    /// The secret for the exchange and whether the exchange is doomed, from `scram_init`. A role with no secret, an MD5 secret or a secret that does not parse gets the mock secret, and the exchange goes on to the end and fails there.
    pub fn secret_for(
        crypto: &(impl Crypto + ?Sized),
        user: &[u8],
        stored: Option<&[u8]>,
        mock_nonce: &[u8; MOCK_NONCE_LEN],
        iterations: i32,
    ) -> (ScramSecret, bool) {
        match stored.and_then(ScramSecret::parse) {
            Some(secret) => (secret, false),
            None => (ScramSecret::mock(crypto, user, mock_nonce, iterations), true),
        }
    }

    /// Reads `SASLInitialResponse` and writes the answer into `out`.
    ///
    /// `certificate_hash` is `Some` on a TLS connection: the hash of the server certificate that `tls-server-end-point` uses. `nonce` is random bytes from the server.
    pub fn start(
        crypto: &(impl Crypto + ?Sized),
        body: &[u8],
        certificate_hash: Option<&[u8]>,
        (secret, doomed): (ScramSecret, bool),
        nonce: [u8; SCRAM_NONCE_LEN],
        out: &mut OutBuf,
    ) -> Result<(Scram, Exchange), ProtocolError> {
        let mut reader = Reader::new(body);
        let mechanism = reader.string().map_err(fatal)?;
        let plus = if mechanism == SCRAM_SHA_256_PLUS && certificate_hash.is_some() {
            true
        } else if mechanism == SCRAM_SHA_256 {
            false
        } else {
            return Err(ProtocolError::fatal(
                "client selected an invalid SASL authentication mechanism",
            ));
        };
        let mut scram = Scram {
            stage: Stage::Init,
            certificate_hash: certificate_hash.map(<[u8]>::to_vec),
            plus,
            secret,
            doomed,
            nonce,
            flag: 0,
            client_first_bare: Vec::new(),
            client_nonce: Vec::new(),
            server_nonce: String::new(),
            server_first: Vec::new(),
            client_key: [0; 32],
        };
        let len = reader.i32().map_err(fatal)?;
        let input = if len == -1 { None } else { Some(reader.bytes(len).map_err(fatal)?) };
        reader.end().map_err(fatal)?;
        let exchange = match input {
            // No initial response: an empty challenge asks the client for the first message.
            None => {
                out.authentication_sasl_continue(&[]);
                Exchange::Continue
            }
            Some(input) => scram.exchange(crypto, input, out)?,
        };
        Ok((scram, exchange))
    }

    /// Reads a `SASLResponse` and writes the answer into `out`.
    pub fn next(
        &mut self,
        crypto: &(impl Crypto + ?Sized),
        body: &[u8],
        out: &mut OutBuf,
    ) -> Result<Exchange, ProtocolError> {
        self.exchange(crypto, body, out)
    }

    /// `ClientKey` and `ServerKey` after a successful exchange. PostgreSQL keeps them so that `postgres_fdw` and `dblink` can log in to another server as the same user.
    pub fn keys(&self) -> Option<([u8; 32], [u8; 32])> {
        (self.stage == Stage::Finished).then_some((self.client_key, self.secret.server_key))
    }

    /// `scram_exchange`.
    fn exchange(
        &mut self,
        crypto: &(impl Crypto + ?Sized),
        input: &[u8],
        out: &mut OutBuf,
    ) -> Result<Exchange, ProtocolError> {
        if input.is_empty() {
            return Err(malformed("The message is empty."));
        }
        if input.contains(&0) {
            return Err(malformed("Message length does not match input length."));
        }
        match self.stage {
            Stage::Init => {
                self.client_first(input)?;
                self.server_nonce = base64::encode(&self.nonce);
                self.server_first = [
                    b"r=",
                    &self.client_nonce[..],
                    self.server_nonce.as_bytes(),
                    b",s=",
                    self.secret.salt.as_bytes(),
                    format!(",i={}", self.secret.iterations).as_bytes(),
                ]
                .concat();
                out.authentication_sasl_continue(&self.server_first);
                self.stage = Stage::SaltSent;
                Ok(Exchange::Continue)
            }
            Stage::SaltSent => {
                let ClientFinal { nonce: final_nonce, proof, without_proof } =
                    self.client_final(input)?;
                let expected = [&self.client_nonce[..], self.server_nonce.as_bytes()].concat();
                if !same(final_nonce, &expected) {
                    return Err(ProtocolError::fatal("invalid SCRAM response")
                        .with_detail("Nonce does not match."));
                }
                let message: [&[u8]; 5] =
                    [&self.client_first_bare, b",", &self.server_first, b",", without_proof];
                // The proof is checked even when the exchange is doomed, so that the time does not tell the client whether the role exists.
                let signature = crypto.hmac_sha256(&self.secret.stored_key, &message);
                for (key, (p, s)) in self.client_key.iter_mut().zip(proof.iter().zip(signature)) {
                    *key = p ^ s;
                }
                let stored_key = crypto.sha256(&[&self.client_key]);
                if !same(&stored_key, &self.secret.stored_key) || self.doomed {
                    return Ok(Exchange::Failure);
                }
                let signature = crypto.hmac_sha256(&self.secret.server_key, &message);
                let verifier = format!("v={}", base64::encode(&signature));
                out.authentication_sasl_final(verifier.as_bytes());
                self.stage = Stage::Finished;
                Ok(Exchange::Success)
            }
            Stage::Finished => Err(ProtocolError::new(
                Level::Fatal,
                INTERNAL_ERROR,
                "invalid SCRAM exchange state",
            )),
        }
    }

    /// `read_client_first_message`.
    fn client_first(&mut self, input: &[u8]) -> Result<(), ProtocolError> {
        let mut text = Cursor { text: input, at: 0 };
        self.flag = text.peek();
        match self.flag {
            flag @ (b'n' | b'y') => {
                if self.plus {
                    return Err(malformed(
                        "The client selected SCRAM-SHA-256-PLUS, but the SCRAM message does not \
                         include channel binding data.",
                    ));
                }
                if flag == b'y' && self.certificate_hash.is_some() {
                    return Err(ProtocolError::new(
                        Level::Fatal,
                        INVALID_AUTHORIZATION,
                        "SCRAM channel binding negotiation error",
                    )
                    .with_detail(
                        "The client supports SCRAM channel binding but thinks the server does \
                         not.  However, this server does support channel binding.",
                    ));
                }
                text.at += 1;
                if text.peek() != b',' {
                    return Err(malformed(format!(
                        "Comma expected, but found character \"{}\".",
                        sanitize_char(text.peek())
                    )));
                }
                text.at += 1;
            }
            b'p' => {
                if !self.plus {
                    return Err(malformed(
                        "The client selected SCRAM-SHA-256 without channel binding, but the \
                         SCRAM message includes channel binding data.",
                    ));
                }
                let kind = text.attr(b'p')?;
                if kind != b"tls-server-end-point" {
                    return Err(ProtocolError::fatal(format!(
                        "unsupported SCRAM channel-binding type \"{}\"",
                        sanitize_str(kind)
                    )));
                }
            }
            flag => {
                return Err(malformed(format!(
                    "Unexpected channel-binding flag \"{}\".",
                    sanitize_char(flag)
                )));
            }
        }
        if text.peek() == b'a' {
            return Err(ProtocolError::new(
                Level::Fatal,
                FEATURE_NOT_SUPPORTED,
                "client uses authorization identity, but it is not supported",
            ));
        }
        if text.peek() != b',' {
            return Err(malformed(format!(
                "Unexpected attribute \"{}\" in client-first-message.",
                sanitize_char(text.peek())
            )));
        }
        text.at += 1;
        self.client_first_bare = input[text.at..].to_vec();
        if text.peek() == b'm' {
            return Err(ProtocolError::new(
                Level::Fatal,
                FEATURE_NOT_SUPPORTED,
                "client requires an unsupported SCRAM extension",
            ));
        }
        // PostgreSQL takes the user from the startup packet and ignores this one.
        text.attr(b'n')?;
        let nonce = text.attr(b'r')?;
        if !nonce.iter().all(|&b| (0x21..=0x7e).contains(&b) && b != b',') {
            return Err(ProtocolError::fatal("non-printable characters in SCRAM nonce"));
        }
        self.client_nonce = nonce.to_vec();
        while text.peek() != 0 {
            text.any_attr()?;
        }
        Ok(())
    }

    /// `read_client_final_message`.
    fn client_final<'a>(&self, input: &'a [u8]) -> Result<ClientFinal<'a>, ProtocolError> {
        let mut text = Cursor { text: input, at: 0 };
        let binding = text.attr(b'c')?;
        if let Some(hash) = self.certificate_hash.as_deref().filter(|_| self.plus) {
            let expected = base64::encode(&[b"p=tls-server-end-point,,", hash].concat());
            if binding != expected.as_bytes() {
                return Err(ProtocolError::new(
                    Level::Fatal,
                    INVALID_AUTHORIZATION,
                    "SCRAM channel binding check failed",
                ));
            }
        } else if !(binding == b"biws" && self.flag == b'n'
            || binding == b"eSws" && self.flag == b'y')
        {
            return Err(ProtocolError::fatal(
                "unexpected SCRAM channel-binding attribute in client-final-message",
            ));
        }
        let nonce = text.attr(b'r')?;
        // Extensions are skipped up to the proof, which must come last.
        let (proof_at, proof) = loop {
            let at = text.at - 1;
            if let (b'p', value) = text.any_attr()? {
                break (at, value);
            }
        };
        let Some(proof) = base64::decode(proof).and_then(|proof| <[u8; 32]>::try_from(proof).ok())
        else {
            return Err(malformed("Malformed proof in client-final-message."));
        };
        if text.peek() != 0 {
            return Err(malformed("Garbage found at the end of client-final-message."));
        }
        Ok(ClientFinal { nonce, proof, without_proof: &input[..proof_at] })
    }
}

/// The parts of the client-final message that the checks use.
struct ClientFinal<'a> {
    nonce: &'a [u8],
    proof: [u8; 32],
    without_proof: &'a [u8],
}

/// A cursor over a SCRAM message. The message has no zero byte, so the cursor gives zero at the end, as the C string in PostgreSQL does.
struct Cursor<'a> {
    text: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn peek(&self) -> u8 {
        self.text.get(self.at).copied().unwrap_or(0)
    }

    /// The value after `=`, up to the next comma, which it skips.
    fn value(&mut self) -> &'a [u8] {
        let start = self.at;
        while !matches!(self.peek(), 0 | b',') {
            self.at += 1;
        }
        let value = &self.text[start..self.at];
        if self.peek() == b',' {
            self.at += 1;
        }
        value
    }

    /// `read_attr_value`.
    fn attr(&mut self, attr: u8) -> Result<&'a [u8], ProtocolError> {
        if self.peek() != attr {
            return Err(malformed(format!(
                "Expected attribute \"{}\" but found \"{}\".",
                char::from(attr),
                sanitize_char(self.peek())
            )));
        }
        self.at += 1;
        self.equals(attr)?;
        Ok(self.value())
    }

    /// `read_any_attr`.
    fn any_attr(&mut self) -> Result<(u8, &'a [u8]), ProtocolError> {
        let attr = self.peek();
        if attr == 0 {
            return Err(malformed("Attribute expected, but found end of string."));
        }
        if !attr.is_ascii_alphabetic() {
            return Err(malformed(format!(
                "Attribute expected, but found invalid character \"{}\".",
                sanitize_char(attr)
            )));
        }
        self.at += 1;
        self.equals(attr)?;
        Ok((attr, self.value()))
    }

    fn equals(&mut self, attr: u8) -> Result<(), ProtocolError> {
        if self.peek() != b'=' {
            return Err(malformed(format!(
                "Expected character \"=\" for attribute \"{}\".",
                char::from(attr)
            )));
        }
        self.at += 1;
        Ok(())
    }
}

fn malformed(detail: impl Into<String>) -> ProtocolError {
    ProtocolError::fatal("malformed SCRAM message").with_detail(detail)
}

fn fatal(error: ProtocolError) -> ProtocolError {
    ProtocolError { level: Level::Fatal, ..error }
}

fn sanitize_char(c: u8) -> String {
    if (0x21..=0x7e).contains(&c) { format!("'{}'", char::from(c)) } else { format!("0x{c:02x}") }
}

fn sanitize_str(text: &[u8]) -> String {
    text.iter()
        .take(30)
        .map(|&c| if (0x21..=0x7e).contains(&c) { char::from(c) } else { '?' })
        .collect()
}

/// A compare that takes the same time wherever the first difference is, as `timingsafe_bcmp`.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// `strsep` for one delimiter.
fn split_once(text: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let at = text.iter().position(|&b| b == delimiter)?;
    Some((&text[..at], &text[at + 1..]))
}

/// `strtol` in base 10 on all of `text`, then the cast to `int`. An empty text is zero, a text with no digits or with bytes after them fails, and a value outside `long` fails.
fn strtol(text: &[u8]) -> Option<i32> {
    if text.is_empty() {
        return Some(0);
    }
    let mut at = text.iter().position(|b| !matches!(b, b' ' | b'\t'..=b'\r')).unwrap_or(text.len());
    let negative = text.get(at) == Some(&b'-');
    if matches!(text.get(at), Some(b'-' | b'+')) {
        at += 1;
    }
    let digits = &text[at..];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut value: i64 = 0;
    for digit in digits {
        let digit = i64::from(digit - b'0');
        value = value.checked_mul(10)?;
        value = if negative { value.checked_sub(digit)? } else { value.checked_add(digit)? };
    }
    Some(value as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::soft::{Soft, hex};
    use crate::{Authentication, Backend};

    // RFC 7677, section 3.
    const CLIENT_FIRST: &[u8] = b"n,,n=user,r=rOprNGfwEbeRWgbNEkqO";
    const CLIENT_FINAL: &[u8] = b"c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
        p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";

    fn rfc_secret() -> ScramSecret {
        let salt = base64::decode(b"W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        ScramSecret::build(&Soft, b"pencil", &salt, 4096)
    }

    /// A `Scram` in the state after the client-first message of RFC 7677. The server nonce of the RFC is not base64 of 18 bytes, so the test puts it in by hand.
    fn after_first(secret: ScramSecret, doomed: bool) -> Scram {
        let mut scram = Scram {
            stage: Stage::Init,
            certificate_hash: None,
            plus: false,
            secret,
            doomed,
            nonce: [0; SCRAM_NONCE_LEN],
            flag: 0,
            client_first_bare: Vec::new(),
            client_nonce: Vec::new(),
            server_nonce: String::new(),
            server_first: Vec::new(),
            client_key: [0; 32],
        };
        scram.client_first(CLIENT_FIRST).unwrap();
        scram.server_nonce = "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0".to_string();
        scram.server_first =
            b"r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096"
                .to_vec();
        scram.stage = Stage::SaltSent;
        scram
    }

    fn initial(mechanism: &[u8], input: Option<&[u8]>) -> Vec<u8> {
        let mut body = [mechanism, b"\0"].concat();
        match input {
            Some(input) => {
                body.extend_from_slice(&(input.len() as i32).to_be_bytes());
                body.extend_from_slice(input);
            }
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
        }
        body
    }

    fn messages(out: &OutBuf) -> Vec<Backend<'_>> {
        let mut rest = out.as_bytes();
        let mut messages = Vec::new();
        while !rest.is_empty() {
            let (message, used) = Backend::decode(rest).unwrap().unwrap();
            messages.push(message);
            rest = &rest[used..];
        }
        messages
    }

    fn start(
        input: &[u8],
        tls: Option<&[u8]>,
        mechanism: &[u8],
    ) -> Result<(Scram, Exchange), ProtocolError> {
        let secret = (rfc_secret(), false);
        Scram::start(
            &Soft,
            &initial(mechanism, Some(input)),
            tls,
            secret,
            [7; 18],
            &mut OutBuf::new(),
        )
    }

    fn detail(error: ProtocolError) -> String {
        assert_eq!(
            (error.level, error.message.as_str()),
            (Level::Fatal, "malformed SCRAM message")
        );
        error.detail.unwrap()
    }

    #[test]
    fn the_rfc_7677_exchange() {
        let secret = rfc_secret();
        let mut scram = after_first(secret.clone(), false);
        let mut out = OutBuf::new();
        assert_eq!(scram.next(&Soft, CLIENT_FINAL, &mut out), Ok(Exchange::Success));
        assert_eq!(
            messages(&out),
            [Backend::Authentication(Authentication::SaslFinal(
                b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="
            ))]
        );
        let (client_key, server_key) = scram.keys().unwrap();
        assert_eq!(Soft.sha256(&[&client_key]), secret.stored_key);
        assert_eq!(server_key, secret.server_key);
    }

    #[test]
    fn a_doomed_exchange_fails_at_the_end() {
        let mut scram = after_first(rfc_secret(), true);
        let mut out = OutBuf::new();
        assert_eq!(scram.next(&Soft, CLIENT_FINAL, &mut out), Ok(Exchange::Failure));
        assert_eq!(out.len(), 0);
        assert_eq!(scram.keys(), None);
        let error = password_failed(b"user");
        assert_eq!(error.sqlstate, "28P01");
        assert_eq!(error.message, "password authentication failed for user \"user\"");
    }

    #[test]
    fn a_wrong_proof_fails() {
        let salt = base64::decode(b"W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let other = ScramSecret::build(&Soft, b"pencil2", &salt, 4096);
        let mut scram = after_first(other, false);
        assert_eq!(scram.next(&Soft, CLIENT_FINAL, &mut OutBuf::new()), Ok(Exchange::Failure));
    }

    #[test]
    fn the_server_first_message() {
        let mut out = OutBuf::new();
        let body = initial(SCRAM_SHA_256, Some(CLIENT_FIRST));
        let (_, exchange) =
            Scram::start(&Soft, &body, None, (rfc_secret(), false), [0xfb; 18], &mut out).unwrap();
        assert_eq!(exchange, Exchange::Continue);
        let expected = format!(
            "r=rOprNGfwEbeRWgbNEkqO{},s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096",
            "+/v7+/v7+/v7+/v7+/v7+/v7"
        );
        assert_eq!(
            messages(&out),
            [Backend::Authentication(Authentication::SaslContinue(expected.as_bytes()))]
        );
    }

    #[test]
    fn no_initial_response_gets_an_empty_challenge() {
        let mut out = OutBuf::new();
        let body = initial(SCRAM_SHA_256, None);
        let (mut scram, exchange) =
            Scram::start(&Soft, &body, None, (rfc_secret(), false), [0; 18], &mut out).unwrap();
        assert_eq!(exchange, Exchange::Continue);
        assert_eq!(messages(&out), [Backend::Authentication(Authentication::SaslContinue(b""))]);
        assert_eq!(scram.next(&Soft, CLIENT_FIRST, &mut OutBuf::new()), Ok(Exchange::Continue));
    }

    #[test]
    fn the_initial_response_message() {
        let error = |body: &[u8]| {
            Scram::start(&Soft, body, None, (rfc_secret(), false), [0; 18], &mut OutBuf::new())
                .unwrap_err()
        };
        let check = |body: &[u8], message: &str| {
            let error = error(body);
            assert_eq!((error.level, error.sqlstate), (Level::Fatal, "08P01"));
            assert_eq!(error.message, message);
        };
        check(b"SCRAM-SHA-256", "invalid string in message");
        // The mechanism is checked before the length.
        check(b"PLAIN\0", "client selected an invalid SASL authentication mechanism");
        check(
            &initial(SCRAM_SHA_256_PLUS, None),
            "client selected an invalid SASL authentication mechanism",
        );
        check(b"SCRAM-SHA-256\0\0\0", "insufficient data left in message");
        check(b"SCRAM-SHA-256\0\xff\xff\xff\xfe", "insufficient data left in message");
        check(&[&initial(SCRAM_SHA_256, None)[..], b"x"].concat(), "invalid message format");
        assert_eq!(detail(error(&initial(SCRAM_SHA_256, Some(b"")))), "The message is empty.");
        assert_eq!(
            detail(error(&initial(SCRAM_SHA_256, Some(b"n,,\0")))),
            "Message length does not match input length."
        );
    }

    #[test]
    fn the_client_first_message() {
        let no_tls = |input: &[u8]| start(input, None, SCRAM_SHA_256).unwrap_err();
        assert_eq!(detail(no_tls(b"x,,n=,r=a")), "Unexpected channel-binding flag \"'x'\".");
        assert_eq!(detail(no_tls(b"n;,n=,r=a")), "Comma expected, but found character \"';'\".");
        assert_eq!(detail(no_tls(b"n")), "Comma expected, but found character \"0x00\".");
        assert_eq!(
            detail(no_tls(b"p=tls-server-end-point,,n=,r=a")),
            "The client selected SCRAM-SHA-256 without channel binding, but the SCRAM message \
             includes channel binding data."
        );
        let error = no_tls(b"n,a=x,n=,r=a");
        assert_eq!(
            (error.sqlstate, error.message.as_str()),
            ("0A000", "client uses authorization identity, but it is not supported")
        );
        assert_eq!(
            detail(no_tls(b"n,b,n=,r=a")),
            "Unexpected attribute \"'b'\" in client-first-message."
        );
        let error = no_tls(b"n,,m=x,n=,r=a");
        assert_eq!(
            (error.sqlstate, error.message.as_str()),
            ("0A000", "client requires an unsupported SCRAM extension")
        );
        assert_eq!(detail(no_tls(b"n,,r=a")), "Expected attribute \"n\" but found \"'r'\".");
        assert_eq!(detail(no_tls(b"n,,n,r=a")), "Expected character \"=\" for attribute \"n\".");
        assert_eq!(detail(no_tls(b"n,,n=")), "Expected attribute \"r\" but found \"0x00\".");
        assert_eq!(no_tls(b"n,,n=,r=a\x7f").message, "non-printable characters in SCRAM nonce");
        assert_eq!(
            detail(no_tls(b"n,,n=,r=a,,")),
            "Attribute expected, but found invalid character \"','\"."
        );
        // The comma after the last value is skipped, so a message may end with one.
        assert!(start(b"n,,n=,r=a,", None, SCRAM_SHA_256).is_ok());
        assert_eq!(
            detail(no_tls(b"n,,n=,r=a,1=x")),
            "Attribute expected, but found invalid character \"'1'\"."
        );
        assert_eq!(detail(no_tls(b"n,,n=,r=a,x")), "Expected character \"=\" for attribute \"x\".");
        assert!(start(b"n,,n=,r=a,x=1,y=", None, SCRAM_SHA_256).is_ok());
        assert!(start(b"y,,n=,r=a", None, SCRAM_SHA_256).is_ok());

        let tls = |input: &[u8], mechanism| start(input, Some(b"hash"), mechanism).unwrap_err();
        let error = tls(b"y,,n=,r=a", SCRAM_SHA_256);
        assert_eq!(
            (error.sqlstate, error.message.as_str()),
            ("28000", "SCRAM channel binding negotiation error")
        );
        assert_eq!(
            detail(tls(b"n,,n=,r=a", SCRAM_SHA_256_PLUS)),
            "The client selected SCRAM-SHA-256-PLUS, but the SCRAM message does not include \
             channel binding data."
        );
        assert_eq!(
            tls(b"p=tls-unique-and-a-very-long-name\x01,,n=,r=a", SCRAM_SHA_256_PLUS).message,
            "unsupported SCRAM channel-binding type \"tls-unique-and-a-very-long-nam\"",
        );
        assert_eq!(
            detail(tls(b"p", SCRAM_SHA_256_PLUS)),
            "Expected character \"=\" for attribute \"p\"."
        );
        assert!(
            start(b"p=tls-server-end-point,,n=,r=a", Some(b"hash"), SCRAM_SHA_256_PLUS).is_ok()
        );
        assert!(start(b"n,,n=,r=a", Some(b"hash"), SCRAM_SHA_256).is_ok());
    }

    #[test]
    fn the_client_final_message() {
        let error = |input: &[u8]| {
            after_first(rfc_secret(), false).next(&Soft, input, &mut OutBuf::new()).unwrap_err()
        };
        let proof = "p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
        let nonce = "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
        assert_eq!(
            error(format!("c=eSws,{nonce},{proof}").as_bytes()).message,
            "unexpected SCRAM channel-binding attribute in client-final-message"
        );
        assert_eq!(
            detail(error(format!("c=biws,{nonce}").as_bytes())),
            "Attribute expected, but found end of string."
        );
        assert_eq!(
            detail(error(format!("c=biws,{nonce},p=abc").as_bytes())),
            "Malformed proof in client-final-message."
        );
        assert_eq!(
            detail(error(format!("c=biws,{nonce},{proof},x=1").as_bytes())),
            "Garbage found at the end of client-final-message."
        );
        let wrong = error(format!("c=biws,r=rOprNGfwEbeRWgbNEkqO,{proof}").as_bytes());
        assert_eq!(
            (wrong.message.as_str(), wrong.detail.as_deref()),
            ("invalid SCRAM response", Some("Nonce does not match."))
        );
        // An extension before the proof is skipped, and it is part of the signed message.
        let mut scram = after_first(rfc_secret(), false);
        let input = format!("c=biws,{nonce},x=1,{proof}");
        assert_eq!(scram.next(&Soft, input.as_bytes(), &mut OutBuf::new()), Ok(Exchange::Failure));
    }

    #[test]
    fn channel_binding() {
        let hash = b"0123456789abcdef0123456789abcdef";
        let mut out = OutBuf::new();
        let body = initial(SCRAM_SHA_256_PLUS, Some(b"p=tls-server-end-point,,n=,r=abc"));
        let (mut scram, _) =
            Scram::start(&Soft, &body, Some(hash), (rfc_secret(), false), [0; 18], &mut out)
                .unwrap();
        let nonce = format!("r=abc{}", base64::encode(&[0; 18]));
        let input = format!("c=biws,{nonce},p={}", base64::encode(&[0; 32]));
        let error = scram.clone().next(&Soft, input.as_bytes(), &mut OutBuf::new()).unwrap_err();
        assert_eq!(
            (error.sqlstate, error.message.as_str()),
            ("28000", "SCRAM channel binding check failed")
        );
        let binding = base64::encode(&[&b"p=tls-server-end-point,,"[..], hash].concat());
        let input = format!("c={binding},{nonce},p={}", base64::encode(&[0; 32]));
        assert_eq!(scram.next(&Soft, input.as_bytes(), &mut OutBuf::new()), Ok(Exchange::Failure));
    }

    #[test]
    fn secrets() {
        let secret = rfc_secret();
        let text = secret.to_string();
        assert_eq!(ScramSecret::parse(text.as_bytes()), Some(secret.clone()));
        assert_eq!(PasswordType::of(text.as_bytes()), PasswordType::ScramSha256);
        assert_eq!(PasswordType::of(b"md5d41d8cd98f00b204e9800998ecf8427e"), PasswordType::Md5);
        assert_eq!(
            PasswordType::of(b"md5D41d8cd98f00b204e9800998ecf8427e"),
            PasswordType::Plaintext
        );
        assert_eq!(
            PasswordType::of(b"md5d41d8cd98f00b204e9800998ecf8427"),
            PasswordType::Plaintext
        );
        let keys = "$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        let parse = |head: &str| ScramSecret::parse(format!("{head}{keys}").as_bytes());
        assert_eq!(parse("SCRAM-SHA-256$:c2FsdA==").unwrap().iterations, 0);
        assert_eq!(parse("SCRAM-SHA-256$ -12:").unwrap().iterations, -12);
        assert_eq!(parse("SCRAM-SHA-256$4294967297:").unwrap().iterations, 1);
        assert_eq!(parse("SCRAM-SHA-256$99999999999999999999:"), None);
        assert_eq!(parse("SCRAM-SHA-256$ :"), None);
        assert_eq!(parse("SCRAM-SHA-256$12 :"), None);
        assert_eq!(parse("SCRAM-SHA-256$12:c2Fsd"), None);
        assert_eq!(parse("SCRAM-SHA-512$12:"), None);
        assert_eq!(ScramSecret::parse(b"SCRAM-SHA-256$12:$AAAA:AAAA"), None);
    }

    #[test]
    fn mock_secret() {
        let nonce = [9; MOCK_NONCE_LEN];
        let (secret, doomed) = Scram::secret_for(&Soft, b"nobody", None, &nonce, 4096);
        assert!(doomed);
        assert_eq!(secret, ScramSecret::mock(&Soft, b"nobody", &nonce, 4096));
        assert_eq!(
            base64::decode(secret.salt.as_bytes()).unwrap(),
            Soft.sha256(&[b"nobody", &nonce])[..16]
        );
        let md5 = b"md5d41d8cd98f00b204e9800998ecf8427e";
        assert!(Scram::secret_for(&Soft, b"nobody", Some(md5), &nonce, 4096).1);
        let stored = rfc_secret().to_string();
        assert_eq!(
            Scram::secret_for(&Soft, b"u", Some(stored.as_bytes()), &nonce, 1),
            (rfc_secret(), false)
        );
    }

    #[test]
    fn password_messages() {
        assert_eq!(password_message(b"secret\0"), Ok(&b"secret"[..]));
        assert_eq!(password_message(b"\0").unwrap_err().sqlstate, "28P01");
        for body in [&b""[..], b"secret", b"sec\0ret\0", b"secret\0\0"] {
            assert_eq!(password_message(body).unwrap_err().message, "invalid password packet size");
        }
    }

    #[test]
    fn md5() {
        // The secret of `CREATE ROLE alice PASSWORD 'secret'` with password_encryption = md5.
        let secret = md5_encrypt(&Soft, b"secret", b"alice");
        assert_eq!(secret, format!("md5{}", hex(&Soft.md5(&[b"secretalice"]))));
        let salt = [1, 2, 3, 4];
        let response = md5_encrypt(&Soft, &secret.as_bytes()[3..], &salt);
        assert!(verify_md5(&Soft, secret.as_bytes(), response.as_bytes(), salt));
        assert!(!verify_md5(&Soft, secret.as_bytes(), response.as_bytes(), [0; 4]));
        assert!(!verify_md5(&Soft, b"secret", response.as_bytes(), salt));
        assert!(verify_password(&Soft, b"alice", secret.as_bytes(), b"secret"));
        assert!(!verify_password(&Soft, b"bob", secret.as_bytes(), b"secret"));
    }

    #[test]
    fn clear_text_passwords() {
        let secret = rfc_secret().to_string();
        assert!(verify_password(&Soft, b"user", secret.as_bytes(), b"pencil"));
        assert!(!verify_password(&Soft, b"user", secret.as_bytes(), b"pencil2"));
        assert!(!verify_password(&Soft, b"user", b"pencil", b"pencil"));
    }

    #[test]
    fn mechanisms() {
        let mut out = OutBuf::new();
        out.authentication_sasl(Scram::mechanisms(true));
        assert!(out.as_bytes().ends_with(b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0\0"));
        assert_eq!(Scram::mechanisms(false), [SCRAM_SHA_256]);
    }
}
