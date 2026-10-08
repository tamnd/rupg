//! The `type_input` target of spec/21 section 21.7. See [`rupg_fuzz::types::type_input`].

#![no_main]
// The fuzz_target macro of libfuzzer-sys uses std::fs::File to write a debug file.
#![allow(clippy::disallowed_types)]

use libfuzzer_sys::fuzz_target;
use rupg_fuzz::types::InputCase;

fuzz_target!(|case: InputCase| rupg_fuzz::types::type_input(&case));
