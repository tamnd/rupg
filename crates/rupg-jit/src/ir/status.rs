// Lifted from rudb crates/rudb-qc-ir/src/status.rs at cd9f9676 (2026-10-05).
// Changes: none.

//! The status a pipeline step returns, per section 5.4.1 of `spec/compiler/05-pipelines-and-state.md`:
//! the kind in the low 8 bits and a payload in the high 56.
//!
//! Every tier builds these values the same way, so the driver reads one encoding whatever ran
//! the step. The constants are what generated code and the interpreter build a status from, and
//! [`Status`] is how the driver reads one back.

use std::fmt;

/// The morsel is done; go on.
pub const OK: u64 = 0;
/// Call the body again with the same morsel; the cursor is in local state.
pub const YIELD: u64 = 1;
/// The pipeline may stop early: a LIMIT was reached.
pub const DONE: u64 = 2;
/// A guard failed. The payload is the guard id.
pub const DEOPT: u64 = 3;
/// The body needs memory. The payload is the slot id.
pub const NEED_MEMORY: u64 = 4;
/// The query is being cancelled.
pub const CANCELLED: u64 = 5;
/// The query fails. The payload is the error site id.
pub const ERROR: u64 = 6;

/// A status of kind `kind` carrying `payload`.
#[must_use]
pub fn make(kind: u64, payload: u64) -> u64 {
    kind | (payload << 8)
}

/// The kind of a status.
#[must_use]
pub fn kind(status: u64) -> u64 {
    status & 0xff
}

/// The payload of a status.
#[must_use]
pub fn payload(status: u64) -> u64 {
    status >> 8
}

/// What a step asks the runtime to do next, one variant per kind of the table in section 5.4.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Go on to the next morsel or the next step.
    Ok,
    /// Call the body again with the same morsel.
    Yield,
    /// The pipeline may stop early.
    Done,
    /// Roll the morsel back and run it again on the fallback variant of the guard site.
    Deopt,
    /// Grant memory to the slot and call the body again.
    NeedMemory,
    /// Stop, the query is being torn down.
    Cancelled,
    /// Abort the query with the error of the site.
    Error,
    /// A kind no step returns, which is a bug in whatever made the status.
    Unknown(u8),
}

/// A step's result as it crosses the ABI: one word, `#[repr(transparent)]` so that a step
/// returning a `u64` and one returning a `Status` are the same function.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Status(pub u64);

impl Status {
    /// The status that says go on.
    pub const OK: Status = Status(OK);

    /// A status of `kind` with `payload`.
    #[must_use]
    pub fn new(kind: u64, payload: u64) -> Status {
        Status(make(kind, payload))
    }

    /// What it asks for.
    #[must_use]
    pub fn kind(self) -> Kind {
        match kind(self.0) {
            OK => Kind::Ok,
            YIELD => Kind::Yield,
            DONE => Kind::Done,
            DEOPT => Kind::Deopt,
            NEED_MEMORY => Kind::NeedMemory,
            CANCELLED => Kind::Cancelled,
            ERROR => Kind::Error,
            other => Kind::Unknown(other as u8),
        }
    }

    /// The payload: the guard site of a `Deopt`, the slot of a `NeedMemory`, the error site of an
    /// `Error`, and zero for the rest.
    #[must_use]
    pub fn payload(self) -> u64 {
        payload(self.0)
    }
}

impl From<u64> for Status {
    fn from(word: u64) -> Status {
        Status(word)
    }
}

impl fmt::Debug for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind() {
            Kind::Ok | Kind::Yield | Kind::Done | Kind::Cancelled => write!(f, "{:?}", self.kind()),
            kind => write!(f, "{kind:?}({})", self.payload()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_reads_back_its_kind_and_payload() {
        let s = Status::new(NEED_MEMORY, 3);
        assert_eq!(s.kind(), Kind::NeedMemory);
        assert_eq!(s.payload(), 3);
        assert_eq!(Status::from(make(ERROR, 7)).kind(), Kind::Error);
        assert_eq!(Status::OK.kind(), Kind::Ok);
        assert_eq!(Status(0x42).kind(), Kind::Unknown(0x42));
        assert_eq!(format!("{:?}", Status::new(DEOPT, 2)), "Deopt(2)");
    }
}
