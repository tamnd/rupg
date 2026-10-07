//! Replay the seeds of the two fuzz targets of the codec and every input that they found.
//!
//! The fuzzer needs nightly and libFuzzer, which not every machine that runs `cargo test` has. This runs the same checks once on the inputs that matter, so a seed that stops working or a bug that the fuzzer found once fails the ordinary test suite.
//!
//! Lifted from `crates/rudb-pgwire/tests/fuzz_inputs.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

// The test reads the seed files of the repository, which are not database files, so it uses std::fs and not the Io trait of rupg-platform.
#![allow(clippy::disallowed_methods)]

#[path = "support/server.rs"]
mod server;

use std::path::{Path, PathBuf};

#[test]
fn every_fuzzer_seed_gives_the_same_output_in_chunks() {
    let files = files_in(&root().join("fuzz/seeds/wire_startup"));
    assert!(files.len() >= 8, "the seed corpus of wire_startup has {} files", files.len());
    for file in files {
        replay(&file);
    }
}

#[test]
fn good_pipelines_answer_once_for_each_sync() {
    // The seeds with only good messages, or with bad ones that the skip drops, also pass the checks of the `wire_frontend` target.
    let good = [
        "simple_query",
        "extended_unnamed",
        "copy_in",
        "copy_fail",
        "skip_to_sync",
        "named_and_long",
        "bad_value_and_pipeline",
        "function_call",
    ];
    for name in good {
        let file = root().join("fuzz/seeds/wire_startup").join(name);
        let bytes = std::fs::read(&file)
            .unwrap_or_else(|why| panic!("cannot read {}: {why}", file.display()));
        let server = server::exercise(bytes[0], &bytes[1..]);
        assert!(server.events.contains(&server::Event::Ready), "{name}");
        server.check_ready();
    }
}

#[test]
fn every_input_the_fuzzer_found_stays_fixed() {
    // A crasher of the `wire_startup` target is committed to this directory as it is. The directory is read and not listed in code, so committing the file is all the work of a regression case.
    for file in files_in(&root().join("crates/rupg-wire/tests/crashers")) {
        replay(&file);
    }
}

fn replay(file: &Path) {
    let bytes =
        std::fs::read(file).unwrap_or_else(|why| panic!("cannot read {}: {why}", file.display()));
    if let Some((&chunk, input)) = bytes.split_first() {
        server::exercise(chunk, input);
    }
}

/// Every file in a directory, sorted, so a failure names the same file on every machine.
fn files_in(directory: &Path) -> Vec<PathBuf> {
    let entries = std::fs::read_dir(directory)
        .unwrap_or_else(|why| panic!("cannot read {}: {why}", directory.display()));
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.expect("cannot read a directory entry").path())
        .filter(|path| path.is_file() && path.file_name().is_some_and(|name| name != ".gitkeep"))
        .collect();
    files.sort();
    files
}

/// The top of the repository, two levels up from this crate.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}
