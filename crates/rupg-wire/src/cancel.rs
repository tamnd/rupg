//! The cancel key of a session, and the check of a `CancelRequest` against it.
//!
//! The server sends the key in `BackendKeyData` at the end of the startup. To cancel a query, a client opens a new connection and sends the process ID and the key in a `CancelRequest`. The codec has no random source, so the server gives the random bytes, and the codec gives the length, the check and the log lines of PostgreSQL.
//!
//! Lifted from `crates/rudb-pgwire/src/cancel.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use crate::backend::OutBuf;
use crate::frame::{Cancel, PROTOCOL_3_2};

/// The length of the cancel key in protocol 3.2, which is `MAX_CANCEL_KEY_LENGTH` in `procsignal.h`. Protocol 3.0 and 3.1 have a key of 4 bytes.
pub const CANCEL_KEY_LEN: usize = 32;

/// The text and the code of the error that a canceled statement gets.
pub const QUERY_CANCELED: (&str, &str) = ("57014", "canceling statement due to user request");

/// The process ID and the secret key of a session.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CancelKey {
    pid: i32,
    key: [u8; CANCEL_KEY_LEN],
    len: usize,
}

impl std::fmt::Debug for CancelKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key is a secret, so it is not in the logs.
        f.debug_struct("CancelKey").field("pid", &self.pid).field("len", &self.len).finish()
    }
}

impl CancelKey {
    /// A key for a session of version `protocol`, from strong random bytes. PostgreSQL uses all 32 bytes in protocol 3.2 and later, and the first 4 bytes before it.
    ///
    /// # Panics
    ///
    /// When `pid` is 0, because PostgreSQL rejects a `CancelRequest` with the process ID 0.
    pub fn new(pid: i32, protocol: u32, random: [u8; CANCEL_KEY_LEN]) -> CancelKey {
        assert_ne!(pid, 0, "the process ID 0 cannot be canceled");
        let len = if protocol >= PROTOCOL_3_2 { CANCEL_KEY_LEN } else { 4 };
        let mut key = [0; CANCEL_KEY_LEN];
        key[..len].copy_from_slice(&random[..len]);
        CancelKey { pid, key, len }
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn key(&self) -> &[u8] {
        &self.key[..self.len]
    }

    /// Writes `BackendKeyData`.
    pub fn write(&self, out: &mut OutBuf) {
        out.backend_key_data(self.pid, self.key());
    }

    /// Whether `key` is this key. Keys of a different length do not match. For keys of the same length the time does not depend on where the bytes differ, as `timingsafe_bcmp`.
    pub fn matches(&self, key: &[u8]) -> bool {
        key.len() == self.len && self.key().iter().zip(key).fold(0, |d, (a, b)| d | (a ^ b)) == 0
    }
}

/// Finds the session that a `CancelRequest` is for, as `SendCancelRequest` in `procsignal.c` does. `find` gives the key of the session with a process ID, if there is one.
///
/// # Errors
///
/// The line that PostgreSQL writes to its log at level `LOG` when it cancels nothing. The client gets no answer in any case: the server closes the connection.
pub fn cancel_target(
    cancel: &Cancel<'_>,
    find: impl FnOnce(i32) -> Option<CancelKey>,
) -> Result<i32, String> {
    if cancel.pid == 0 {
        return Err("invalid cancel request with PID 0".to_owned());
    }
    match find(cancel.pid) {
        Some(key) if key.matches(cancel.key) => Ok(cancel.pid),
        Some(_) => Err(format!("wrong key in cancel request for process {}", cancel.pid)),
        None => Err(format!("PID {} in cancel request did not match any process", cancel.pid)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::frame::{PROTOCOL_3_0, protocol};

    fn random() -> [u8; CANCEL_KEY_LEN] {
        std::array::from_fn(|i| i as u8 + 1)
    }

    #[test]
    fn the_key_length_follows_the_protocol() {
        // The server at the pin sends 4 bytes for 3.0 and 3.1 and 32 bytes for 3.2.
        assert_eq!(CancelKey::new(7, PROTOCOL_3_0, random()).key(), [1, 2, 3, 4]);
        assert_eq!(CancelKey::new(7, protocol(3, 1), random()).key().len(), 4);
        let key = CancelKey::new(7, PROTOCOL_3_2, random());
        assert_eq!(key.key(), random());
        let mut out = OutBuf::default();
        key.write(&mut out);
        let (message, _) = Backend::decode(out.as_bytes()).unwrap().unwrap();
        assert_eq!(message, Backend::BackendKeyData { pid: 7, key: &random() });
        // The bytes of the key are not in the debug output.
        assert_eq!(format!("{key:?}"), "CancelKey { pid: 7, len: 32 }");
    }

    #[test]
    fn a_cancel_request_matches_one_key() {
        let key = CancelKey::new(42, PROTOCOL_3_2, random());
        let find = |pid| (pid == 42).then_some(key);
        assert_eq!(cancel_target(&Cancel { pid: 42, key: &random() }, find), Ok(42));
        let mut wrong = random();
        wrong[31] ^= 1;
        assert_eq!(
            cancel_target(&Cancel { pid: 42, key: &wrong }, find),
            Err("wrong key in cancel request for process 42".to_owned())
        );
        // A 4-byte key does not match the first 4 bytes of a 32-byte key.
        assert!(cancel_target(&Cancel { pid: 42, key: &random()[..4] }, find).is_err());
        assert_eq!(
            cancel_target(&Cancel { pid: 9, key: &random() }, find),
            Err("PID 9 in cancel request did not match any process".to_owned())
        );
        assert_eq!(
            cancel_target(&Cancel { pid: 0, key: &random() }, |_| panic!()),
            Err("invalid cancel request with PID 0".to_owned())
        );
    }
}
