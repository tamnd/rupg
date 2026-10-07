# Fuzz targets

This directory holds the fuzz targets of spec/21 section 21.7. It is a separate workspace, because `cargo-fuzz` needs the nightly toolchain and the main workspace stays on the pinned stable toolchain.

## Targets

All targets start from the same base file. The base is a database on the simulated disk with two tables. It has 32 commits before a checkpoint and 16 commits after it, and it is not closed, so each open replays the log. The base keeps the rows of the tables after each commit.

| Target | What it changes | Also a failure |
|---|---|---|
| `file_open` | any byte of the file, and the length of the file | an open that gives rows that are not the rows after one of the commits |
| `page_decode` | the bytes after the first 24 bytes of one hot page, with a good checksum | none |
| `log_replay` | the bytes of the log after the redo position | an open that gives rows that are not the rows after a commit at or after the checkpoint, or an error that is not `XX001` |

Each target runs `rupg check` on the changed file, opens the file and reads every table. A panic, a hang over 1 second or memory over 2 GiB is a failure for each target.

The input is a list of changes of 7 bytes each: an offset of 4 bytes in little-endian order, a kind and a value of 2 bytes. The kind sets one byte, sets 2 bytes, sets up to 512 bytes to zero, copies 64 bytes from another place, or cuts the file. Only `file_open` cuts the file.

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

`cargo test` in this directory builds the base file and applies 200 random inputs to each target. The nightly workflow runs each target for 20 minutes.
