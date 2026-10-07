# Fuzz targets

This directory holds the fuzz targets of spec/21 section 21.7. It is a separate workspace, because `cargo-fuzz` needs the nightly toolchain and the main workspace stays on the pinned stable toolchain.

## Targets

### The file

The targets of the file start from the same base file. The base is a database on the simulated disk with two tables. It has 32 commits before a checkpoint and 16 commits after it, and it is not closed, so each open replays the log. The base keeps the rows of the tables after each commit.

| Target | What it changes | Also a failure |
|---|---|---|
| `file_open` | any byte of the file, and the length of the file | an open that gives rows that are not the rows after one of the commits |
| `page_decode` | the bytes after the first 24 bytes of one hot page, with a good checksum | none |
| `log_replay` | the bytes of the log after the redo position | an open that gives rows that are not the rows after a commit at or after the checkpoint, or an error that is not `XX001` |

Each target runs `rupg check` on the changed file, opens the file and reads every table. A panic, a hang over 1 second or memory over 2 GiB is a failure for each target.

The input is a list of changes of 7 bytes each: an offset of 4 bytes in little-endian order, a kind and a value of 2 bytes. The kind sets one byte, sets 2 bytes, sets up to 512 bytes to zero, copies 64 bytes from another place, or cuts the file. Only `file_open` cuts the file.

### The wire

The targets of the wire run the small server of `crates/rupg-wire/tests/support/server.rs` on the codec. It does the handshake with `trust`, the main loop of a session, the prepared statements and the portals by name, with fake statements.

| Target | Input | Also a failure |
|---|---|---|
| `wire_startup` | a chunk size, then the raw bytes that a client sends on one connection: the `SSLRequest`, `GSSENCRequest`, `CancelRequest` or `StartupMessage`, and the messages after it | different output when the bytes come in chunks, a message that does not come back the same after an encode and a parse, a message after a `FATAL` |
| `wire_frontend` | a list of good client messages after a good startup | a `ReadyForQuery` that the protocol does not allow: not one for each `Query`, `FunctionCall` and `Sync`, or a message other than `Sync` and `Terminate` that the skip after an error does not drop |

`seeds/wire_startup` holds the seed inputs of `wire_startup`. `cargo test -p rupg-wire` in the main workspace runs the seeds and every input in `crates/rupg-wire/tests/crashers` through the same checks, so a crash that the fuzzer found stays fixed. At M2 the startup uses `trust`. The SCRAM exchange joins `wire_startup` with `rupg-server`.

## Run a target

Install `cargo-fuzz` and the nightly toolchain:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
```

Run one target from this directory:

```sh
cargo +nightly fuzz run -O -s none file_open -- -timeout=1 -rss_limit_mb=2048
```

The option `-s none` turns off the address sanitizer. The code of these targets has no `unsafe` block of its own, and the targets run 2 to 4 times faster without the sanitizer. Use `-s address` to fuzz with the sanitizer.

To run a failing input again:

```sh
cargo +nightly fuzz run -O -s none file_open artifacts/file_open/crash-<hash>
```

`cargo test` in this directory builds the base file and applies 200 random inputs to each target of the file. The nightly workflow runs each target for 20 minutes.
