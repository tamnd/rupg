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

### The types

The targets of the types call the input, output, receive and send functions of `rupg-types` and `rupg-func` for a value of one of 52 types: the scalar types of M2, some typmods such as `numeric(10,4)` and `timestamptz(3)`, and arrays. The session is in UTC, and its clock is fixed.

| Target | Input | Also a failure |
|---|---|---|
| `type_input` | a type, the settings `DateStyle`, `IntervalStyle`, `extra_float_digits`, `bytea_output` and `array_nulls`, and a text | an internal error that PostgreSQL does not also give, a value that the input takes but the output refuses, an output that does not read back to the same output, or a send and receive that give a different output |
| `type_recv` | a type and the bytes of a binary value | an internal error that PostgreSQL does not also give, a value that does not give the same bytes after a send and a receive, or an output that does not read back to the same output |

A float output with `extra_float_digits` below 1 loses digits, so `type_input` does not read it back. An empty `int2vector` or `oidvector` does not come back from its binary form in PostgreSQL either, so `type_input` does not check its send and receive.

The checks in the loop do not compare with PostgreSQL. The example `type_replay` compares a corpus with the oracle. It writes a psql script, and the script prints one line for each case: the error of `pg_input_error_info`, or the output and the hex of the send function. For `type_recv` it writes each case as a file of `COPY` in the binary format, and the script reads the file with `COPY`. Then `type_replay diff` compares the lines with the lines of rupg:

```sh
cargo +nightly run --release --example type_replay -- sql input corpus/type_input > in.sql
psql -X -At -F "$(printf '\t')" -f in.sql > in.tsv
cargo +nightly run --release --example type_replay -- diff input corpus/type_input in.tsv
cargo +nightly run --release --example type_replay -- sql recv corpus/type_recv "$PWD/copy" > rc.sql
psql -X -At -F "$(printf '\t')" -f rc.sql > rc.tsv
cargo +nightly run --release --example type_replay -- diff recv corpus/type_recv rc.tsv
```

The server reads the files of `COPY`, so the directory must be an absolute path that the server can read, and the user must be a superuser or have `pg_read_server_files`. A case where rupg gives `0A000` counts as not supported. For an input with `now`, `today`, `tomorrow` or `yesterday`, only `OK` or the SQLSTATE of the error counts.

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

`cargo test` in this directory builds the base file and applies 200 random inputs to each target of the file, and 2000 random inputs to `type_input` and `type_recv`. The nightly workflow runs each target for 20 minutes.
