//! The signals that stop the server: `SIGTERM` for the smart shutdown and `SIGINT` for the fast shutdown of PostgreSQL. The signals are blocked in each thread, and one thread takes them with `sigwait`, so no signal handler runs.

use std::io;

use rupg_common::{Error, Result, SqlState};

/// The blocked stop signals of the process. [`StopSignals::block`] must run before the process starts a thread, so that each thread inherits the mask.
pub struct StopSignals {
    set: libc::sigset_t,
}

impl std::fmt::Debug for StopSignals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StopSignals { SIGTERM, SIGINT }")
    }
}

impl StopSignals {
    /// Blocks `SIGTERM` and `SIGINT` in the calling thread and in the threads that it starts after this call.
    ///
    /// # Errors
    ///
    /// `XX000` when the mask of the thread does not change.
    pub fn block() -> Result<StopSignals> {
        // SAFETY: an all-zero `sigset_t` is valid storage, and `sigemptyset` sets it.
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: `set` is a valid `sigset_t`, and the signals are valid signal numbers.
        let made = unsafe {
            libc::sigemptyset(&raw mut set) == 0
                && libc::sigaddset(&raw mut set, libc::SIGTERM) == 0
                && libc::sigaddset(&raw mut set, libc::SIGINT) == 0
        };
        // SAFETY: `set` is a valid signal set, and the old mask is not wanted.
        let masked = made
            && unsafe {
                libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut())
            } == 0;
        if !masked {
            return Err(Error::new(
                SqlState::INTERNAL_ERROR,
                format!("could not block the stop signals: {}", io::Error::last_os_error()),
            ));
        }
        Ok(StopSignals { set })
    }

    /// Waits until the process gets `SIGTERM` or `SIGINT`, and gives the number of the signal.
    pub fn wait(&self) -> i32 {
        loop {
            let mut signal = 0;
            // SAFETY: `self.set` is a valid signal set, and `signal` is a valid place for the result.
            if unsafe { libc::sigwait(&raw const self.set, &raw mut signal) } == 0 {
                return signal;
            }
        }
    }
}
