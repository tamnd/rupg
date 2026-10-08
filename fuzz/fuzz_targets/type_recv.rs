//! The `type_recv` target of spec/21 section 21.7. See [`rupg_fuzz::types::type_recv`].

#![no_main]
// The fuzz_target macro of libfuzzer-sys uses std::fs::File to write a debug file.
#![allow(clippy::disallowed_types)]

use libfuzzer_sys::fuzz_target;
use rupg_fuzz::types::RecvCase;

fuzz_target!(|case: RecvCase| rupg_fuzz::types::type_recv(&case));
