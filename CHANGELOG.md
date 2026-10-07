# Changelog

All changes that a user can see are in this file. The version is 0.M.patch, where M is the last milestone that closed. See [`spec/23-milestones.md`](spec/23-milestones.md) section 23.17.

## Unreleased

### Added

- The page table of spec/08 section 8.5 in `rupg-file`: `PtEntry` with the 48-bit physical page and the 16-bit write tag, `PtPath` for the radix walk, and `PtNode` for the 2040 entries of a node. A table has 3 levels and grows to 6 when the logical page numbers need it.
- The extent classes E0 to E4 with `Extent`, which checks the alignment inside the arena, and the checksum of each 64 KiB block.
- The arenas and the free space map of spec/08 section 8.9: `ArenaMap` with the buddy rule and the in-order pages of a write arena, the 8-byte `FsmEntry`, and `FsmPage` with 120 arenas in each page. A double free and an entry that does not match its bitmap are errors.
- The text form for page table nodes and free space map pages. It parses back into the same sealed page.

## 0.0.2 (2026-10-07)

The second patch release on the 0.0 line. It has the error type and the identifiers of `rupg-common`, the platform traits with their simulation, and the first part of the file format. No crate is published.

### Added

- `Error` and `SqlState` in `rupg-common`. Each error has the SQLSTATE that PostgreSQL sends for the same cause. A test checks each constant against the `errcodes.txt` of the pin.
- The identifiers of spec/04 section 4.7 in `rupg-common`: `Oid`, `ShardId`, `RowId`, `Hlc`, `Xid` and `Lsn`. Each has a text form that parses back. `Hlc::tick` and `Hlc::merge` keep the clock monotonic when the wall clock goes back. `Xid::next` skips the 32-bit values 0, 1 and 2.
- The `Io`, `File`, `Clock` and `Entropy` traits in `rupg-platform`, with the operating system implementations `OsIo`, `OsClock` and `OsEntropy`. On macOS the sync is `F_FULLFSYNC`.
- The simulation implementations `SimIo`, `SimClock`, `SimEntropy` and the seeded generator `SimRng`. `SimIo` keeps the files in memory with the power cut model of spec/21 section 21.9.1: unsynced writes can be lost, written, torn at a 4 KiB or 512 byte boundary, misdirected or reordered. It also injects `EIO` and `ENOSPC`.
- The `Tasks` and `Net` traits with `OsTasks` and `OsNet` (threads and TCP), and `SimTasks` and `SimNet`. `SimTasks` runs the tasks on the calling thread in an order that the seed picks. `SimNet` connects listeners and clients in memory.
- `MemoryPool`, the one memory budget of spec/04 section 4.6. A reservation that does not fit asks a reclaimer for memory, then fails with `53200`. A test mode fails the Nth reservation.
- `CpuFeatures` and `CpuLevel`, which pick the kernel path at run time, and `physical_memory`.
- The four-lane page checksum in `rupg-file`, lifted from rudb. It gives the same result as XXH64 with seed 0, and the tests check the reference vectors.
- The fixed blocks of the `.rupg` file in `rupg-file`: the identity block, the header slots A and B, and the owner record. `choose_slot` takes the valid slot with the higher generation by the rule of spec/08 section 8.2.2 and says why. A file with no valid slot gives `XX001` and names `rupg inspect --salvage`.
- The 64-byte page header with the 16 page kinds. `verify` finds a bad checksum, a misdirected write and a lost write, each with `XX001`.
- The text form of spec/08 section 8.14 for each of these blocks. It has one field on each line, sorted, and it parses back.
- The SQLSTATE `22P02` for a text form that does not parse.
- `clippy.toml` now rejects `std::fs`, `std::net`, `std::thread` and `RandomState::new` outside `rupg-platform`, as spec/22 section 22.1 requires. `xtask` allows them.

### Fixed

- The release workflow ran the Windows build in PowerShell, where `$TARGET` is not the environment variable, so the build had no target and failed. The step now runs in bash. Release 0.0.1 has no binaries for this reason.
- `clippy.toml` named two Unix socket types that do not exist on Windows, and clippy warned on a Windows build. The two entries now allow this.

## 0.0.1 (2026-10-07)

The first patch release on the M0 line. M0 is not closed, so this release has no measured number. It has the workspace, the vendored files of the pin, the LALR(1) check of the grammar and the CI. No crate is published.

### Added

- The workspace with the 41 crates of `spec/22-crate-layout.md`. Most crates are stubs until their milestone.
- `cargo xtask layers`, which checks the layer rule from `xtask/layers.toml`.
- `cargo xtask style`, which checks `forbid(unsafe_code)`, `SAFETY` comments and the writing rules.
- The `rupg` binary with `--version` and `--print-config`.
- `CompatVersion` for PostgreSQL 14 to 19 in `rupg-common`.
- The technical specification in `spec/`.
- `vendor/` with the PostgreSQL files of the pin and of 14.24, 15.19, 16.15, 17.11 and 18.6, the time zone data 2026e and the Unicode character database 17.0.0. Each directory has a `PIN` file with the source, the revision, the license and the SHA-256 of each file.
- `cargo xtask vendor`, which fetches the vendored files, and `cargo xtask vendor --check`, which checks them with no network.
- `cargo xtask grammar`, an LALR(1) generator that reads a bison grammar and checks the conflict counts against `%expect`. It gives the same tables as bison 3.8.2 for `gram.y` (6574 states) and `pl_gram.y` (336 states).
- `cargo xtask version`, which sets the workspace version, the exact internal pins and `Cargo.lock` together.
- CI with the matrix of `spec/21-testing.md` section 21.16, nightly sanitizers and Miri, a release workflow with build provenance, and security scans.
