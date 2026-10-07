//! Feed raw bytes from a client to the codec of `rupg-wire`, through the handshake and the main loop of a session.
//!
//! The input is all the bytes that a client sends on one connection, after a first byte that chooses a chunk size. The target runs the small server of `tests/support/server.rs` in `rupg-wire` on the input two times: once with all the bytes at one time, and once with the bytes in chunks of that size, as a socket can give them. The two runs must write the same bytes and use the same input. A server that acts on a message before it has all of it, or that loses a byte at the edge of a chunk, fails this check.
//!
//! The other checks are in the server. Each message that the session gives must come back the same after an encode and a parse, and the server stops at the first `FATAL`, so nothing can follow it. A panic anywhere in the codec is a failure too. libFuzzer finds an allocation above the limit of a message with `-malloc_limit_mb`.

#![no_main]

use libfuzzer_sys::fuzz_target;

// The target uses only a part of the test server.
#[allow(dead_code)]
#[path = "../../crates/rupg-wire/tests/support/server.rs"]
mod server;

fuzz_target!(|data: &[u8]| {
    let Some((&chunk, input)) = data.split_first() else { return };
    server::exercise(chunk, input);
});
