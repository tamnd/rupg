//! The `page_decode` target of spec/21 section 21.7. See [`rupg_fuzz::page_decode`].

#![no_main]
// The fuzz_target macro of libfuzzer-sys uses std::fs::File to write a debug file.
#![allow(clippy::disallowed_types)]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| rupg_fuzz::page_decode(data));
