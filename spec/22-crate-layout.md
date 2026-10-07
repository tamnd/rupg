# 22. Crate layout

Written 7 October 2026. Pins: Rust 1.98.0, MSRV 1.88.0, rudb at `cd9f9676` (5 October 2026), PostgreSQL `REL_19_STABLE` at `7d3d2db7`. The crate name check on crates.io and the repository check on GitHub ran on 7 October 2026. The line counts of rudb were counted on `cd9f9676`. The line budgets for rupg are budgets, not measurements.

This document gives the crates of rupg, the rank of each crate, what each crate owns, the rules between crates, the outside dependencies, the license policy, the code that rupg lifts from rudb, the vendored files, the features, the build profiles and the toolchain. Document 04 gives the layers. This document maps the layers to crates and makes the layer rule a check in the build.

## 22.1 The rules

**A crate may depend only on crates of a lower rank.** `cargo xtask layers` checks this on every change, from the ranks in `xtask/layers.toml`. This is the rule of rudb, and it makes "the encoding layer cannot see the executor" a fact about the build and not a claim in a document.

**Adding a crate is a design decision.** A new crate needs a rank in `xtask/layers.toml`, and the reviewer of that line reviews the design. The rank is not computed from the dependency graph.

**`unsafe` is allowed in six crates only.** The six are `rupg-platform`, `rupg-buffer`, `rupg-kernels`, `rupg-jit`, `rupg-capi` and `rupg-wasm`. Every other crate has `#![forbid(unsafe_code)]` at the top of `lib.rs`, and `cargo xtask style` checks that the line is present. Every `unsafe` block has a `// SAFETY:` comment that states the invariant. Document 04 section 4.8 lists five of the six. `rupg-wasm` is the sixth, because the WebAssembly exports cross a foreign boundary in the same way as the C API.

**The engine reaches the outside world through `rupg-platform` only.** No crate other than `rupg-platform` calls `std::fs`, `std::net`, `std::thread`, `std::time::SystemTime`, `std::time::Instant` or the operating system's random source. The clippy settings `disallowed-methods` and `disallowed-types` in `clippy.toml` enforce this, and the simulation of document 21 section 21.10 depends on it. The front ends at group 13 are the exception, because they own the process.

**One version for the workspace.** Every crate has the version of the workspace. Internal dependencies use an exact pin, for example `rupg-types = { version = "=0.4.0", path = "crates/rupg-types" }`, as rudb does. A published set of crates is always a set from one commit.

**Few outside dependencies.** With every feature off, the library depends only on `libc` on Unix and four small hash crates. Each other outside dependency is behind a feature, has a reason in a comment in the root `Cargo.toml`, and is in the table of section 22.5.

**No AGPL code and no code from AGPL projects.** A contributor must not read the source of pgrust, ParadeDB, PgDog or Hydra. PostgreSQL's source is under the PostgreSQL License, and a contributor may read it and port from it with a provenance header (section 22.7).

## 22.2 The workspace

```
rupg/
  Cargo.toml              resolver 3, members crates/* and xtask, one version
  rust-toolchain.toml     channel 1.98.0
  clippy.toml             disallowed methods and types
  deny.toml               section 22.6
  crates/
    rupg-common/ ... rupg-cluster/     the 41 crates of section 22.4
  vendor/
    postgres/             files from the PostgreSQL pin, section 22.8
    tzdata/               the IANA time zone database
    unicode/              the Unicode character database
  xtask/
    layers.toml           the ranks
    src/                  the xtask commands, section 22.8
  tests/
    crash/  sim/  resource/            the T8, T9 and T12 tests of document 21
  fuzz/                   its own workspace, nightly, excluded from the main workspace
  .github/workflows/      ci.yml, nightly.yml, release.yml
```

`fuzz/` is a separate workspace because `cargo-fuzz` needs the nightly toolchain and the main workspace is pinned to stable. rudb has the same split.

## 22.3 Ranks

Document 04 gives thirteen layers, and the crates come in fourteen groups numbered 0 to 13. Some groups have crates that must see each other. For example, `rupg-table` joins the hot store and the cold store, which are in the same group. So `xtask/layers.toml` stores a two-digit rank: the group number times ten, plus the order of the crate in its group. A crate may depend on a crate of a lower two-digit rank, which includes a crate earlier in its own group. The group numbers stay the numbers that the other documents use.

| Group | layers.toml rank and crate | Layer of document 04 |
|---|---|---|
| 0 | 0 `rupg-common`, 1 `rupg-types` | shared by all |
| 1 | 10 `rupg-platform`, 11 `rupg-file`, 12 `rupg-wire` | file and platform |
| 2 | 20 `rupg-kernels`, 21 `rupg-encode`, 22 `rupg-buffer`, 23 `rupg-tree` | pages |
| 3 | 30 `rupg-log` | log |
| beside 3 | 31 `rupg-raft` | cluster |
| 4 | 40 `rupg-summary`, 41 `rupg-hot`, 42 `rupg-column`, 43 `rupg-table` | table storage |
| 5 | 50 `rupg-txn` | transactions |
| 6 | 60 `rupg-index`, 61 `rupg-graph`, 62 `rupg-ann` | access methods |
| 7 | 70 `rupg-catalog`, 71 `rupg-pgcatalog`, 72 `rupg-shim` | catalog |
| 8 | 80 `rupg-sql` | SQL front |
| 9 | 90 `rupg-analyze`, 91 `rupg-plan`, 92 `rupg-opt` | planning |
| 10 | 100 `rupg-func`, 101 `rupg-jit`, 102 `rupg-exec`, 103 `rupg-exchange` | execution |
| 11 | 110 `rupg-plpgsql`, 111 `rupg-ext`, 112 `rupg-contrib` | procedures |
| beside 5 to 10 | 115 `rupg-cluster` | cluster |
| 12 | 120 `rupg-session` | sessions |
| 13 | 130 `rupg-server`, 131 `rupg`, 132 `rupg-capi`, 133 `rupg-wasm`, 134 `rupg-import`, 135 `rupg-cli` | front ends |

**Why the cluster crates have ranks.** Document 04 section 4.11 puts the cluster beside the stack: the lower layers call it through traits that they define, and they do not name it. The check still needs a number for each crate. `rupg-raft` is at 31 because it needs only the platform, the file format and the log record format. `rupg-cluster` is at 115 because it implements traits from `rupg-log`, `rupg-txn`, `rupg-catalog` and `rupg-exchange`, so it must be above all four. No crate below 130 depends on `rupg-cluster`. The front ends install it in place of the single-node implementations.

**Why `rupg-func` is below `rupg-exec`.** The executor calls functions, and the compiled tier calls the same functions through its runtime. Both need the function table, and neither may be below the other's caller. So the order in group 10 is the functions, then the compile tier, then the executor that uses both, then the exchange operators that wrap executor pipelines.

**Why the summaries are below the column store.** `rupg-summary` holds the summary structures as plain data built from vectors: the scalar summaries, the frequency synopsis, the gram sets. `rupg-column` stores their bytes inside a segment and calls `rupg-summary` to build and read them. The planner reads them through `rupg-table`. This order lets a test build a synopsis from a literal vector with no segment.

## 22.4 The crates

The table gives each crate its group, what it owns, its main dependencies above group 0, whether it may contain `unsafe`, the milestone where it first ships, and its line budget. The budget counts every line in `src/`, as rudb's count does (section 22.14). The main type names are the names that other documents and the code use.

| Crate | Group | Owns | Depends on | `unsafe` | First | Budget (k lines) |
|---|---|---|---|---|---|---|
| `rupg-common` | 0 | `Error` with its `SqlState`, the identifiers `Oid`, `ShardId`, `RowId`, `Hlc`, `Xid` and `Lsn` of document 04 section 4.7, `CompatVersion` | nothing | no | M1 | 8 |
| `rupg-types` | 0 | the value model, `Datum`, `Vector` of 1024 values, `TypeId`, the text and binary forms of the 197 types, numeric, date and time, JSON, arrays, ranges | `rupg-common` | no | M1 | 45 |
| `rupg-platform` | 1 | the traits `Io`, `Clock`, `Net`, `Tasks` and `Entropy`, their operating system and simulation implementations, memory accounting (`MemoryPool`), CPU feature detection | group 0 | yes | M1 | 12 |
| `rupg-file` | 1 | the bytes of the `.rupg` file: the header slots, the page and extent formats, the free space map, the checksums, `PageId`, and the `PageAccess` trait | `rupg-platform` | no | M1 | 10 |
| `rupg-wire` | 1 | the message codec of protocols 3.0 and 3.2, the SCRAM exchange, the `COPY` sub-protocol, the replication messages | group 0 | no | M2 | 8 |
| `rupg-kernels` | 2 | the vector kernels, lifted from rudb: bit unpacking, frame of reference, delta, filters, comparison, hashing, aggregation, with runtime dispatch | group 0 | yes | M1 | 30 |
| `rupg-encode` | 2 | the encodings of document 09, the chooser, and the codecs: zstd, lz4, pglz and snappy | `rupg-kernels` | no | M4 | 25 |
| `rupg-buffer` | 2 | the buffer manager of document 08 section 8.8: frames, page latches, eviction, the I/O queue, the implementation of `PageAccess` | `rupg-platform`, `rupg-file` | yes | M1 | 8 |
| `rupg-tree` | 2 | a B+ tree over `PageAccess` with optimistic latch coupling, used by the hot store, the catalog, TOAST and the B-tree index | `rupg-file` | no | M1 | 10 |
| `rupg-log` | 3 | the per-worker log rings, the record format, group commit, checkpoints, recovery, the replication stream | group 2, `rupg-file` | no | M1 | 14 |
| `rupg-raft` | beside 3 | one Raft group per shard: the election, the log, snapshots, membership changes | `rupg-platform`, `rupg-file`, `rupg-log` | no | M10 | 12 |
| `rupg-summary` | 4 | the scalar summaries, zone maps, frequency synopses and gram sets of H1 to H4 as plain data | `rupg-kernels` | no | M4 | 10 |
| `rupg-hot` | 4 | the hot row pages, in-place update with undo pointers, the row id map | `rupg-tree`, `rupg-log` | no | M1 | 15 |
| `rupg-column` | 4 | the cold segments, the global dictionaries of document 09 section 9.5, the sorted projections, decoding into vectors | `rupg-encode`, `rupg-kernels`, `rupg-summary` | no | M4 | 20 |
| `rupg-table` | 4 | a table as one object over both stores, TOAST, the system columns, the row id allocator per shard, the scan that merges the stores | `rupg-hot`, `rupg-column` | no | M1 | 14 |
| `rupg-txn` | 5 | MVCC, snapshots, undo, row and advisory locks, SSI, savepoints, two-phase commit, commit timestamps, the mover of document 04 section 4.4 | `rupg-table`, `rupg-log` | no | M1 | 22 |
| `rupg-index` | 6 | the B-tree, hash, GIN, GiST, SP-GiST and BRIN access methods and their operator class interface | `rupg-txn`, `rupg-tree` | no | M6 | 40 |
| `rupg-graph` | 6 | the link index and the CSR adjacency of document 13, lifted from rudb | `rupg-txn` | no | M6 | 12 |
| `rupg-ann` | 6 | the vector index of document 13, which `hnsw` and `ivfflat` map to | `rupg-txn`, `rupg-kernels` | no | M6 | 10 |
| `rupg-catalog` | 7 | the rupg catalog of user objects: schemas, tables, columns, constraints, indexes, roles, privileges | `rupg-txn`, `rupg-tree` | no | M2 | 20 |
| `rupg-pgcatalog` | 7 | the built-in objects with their PostgreSQL OIDs, generated from the vendored `.dat` files, and the 64 virtual catalogs, 86 system views and 65 `information_schema` views | `rupg-catalog` | no | M2 | 30 |
| `rupg-shim` | 7 | the differences of versions 14 to 18 from 19: version strings, catalog columns, settings, behaviors | `rupg-pgcatalog` | no | M9 | 8 |
| `rupg-sql` | 8 | the lexer, the parser from the vendored `gram.y`, the parse tree and its printer | group 0 | no | M2 | 30 |
| `rupg-analyze` | 9 | name resolution, type resolution, the query tree, the rewriter for views and rules | `rupg-sql`, group 7 | no | M2 | 45 |
| `rupg-plan` | 9 | the physical plan, its text form, `EXPLAIN` output (document 15 section 15.9) | `rupg-analyze` | no | M2 | 15 |
| `rupg-opt` | 9 | the optimizer, statistics, `ANALYZE`, the cost model, join order, the use of summaries | `rupg-plan` | no | M3 | 40 |
| `rupg-func` | 10 | the 3,414 built-in functions, the 805 operators, the 249 casts, the aggregates and window functions, the regular expression engine | `rupg-pgcatalog`, `rupg-catalog` | no | M2 | 120 |
| `rupg-jit` | 10 | the compile tiers of document 14, lifted from rudb's query compiler | `rupg-func`, `rupg-plan` | yes | M6 | 30 |
| `rupg-exec` | 10 | the vectorized executor, the point path, compressed execution, spill, the per-distinct result tables of document 14 section 14.6 | `rupg-jit`, `rupg-plan`, group 6 | no | M2 | 60 |
| `rupg-exchange` | 10 | the exchange operators between workers and between nodes | `rupg-exec` | no | M8 | 8 |
| `rupg-plpgsql` | 11 | PL/pgSQL, trigger execution, event triggers | group 10 | no | M7 | 25 |
| `rupg-ext` | 11 | the extension API, `CREATE EXTENSION` for built-in extensions | group 10 | no | M6 | 8 |
| `rupg-contrib` | 11 | the contrib extensions that document 16 lists, in Rust | `rupg-ext` | no | M6 | 50 |
| `rupg-cluster` | beside 5 to 10 | placement, the shard map, distributed commit, distributed query, rebalancing | `rupg-raft`, `rupg-txn`, `rupg-catalog`, `rupg-exchange` | no | M10 | 25 |
| `rupg-session` | 12 | sessions, the 438 settings, portals, prepared statements, the plan cache, `LISTEN` and `NOTIFY` | group 11 | no | M2 | 15 |
| `rupg-server` | 13 | the listener, TLS, authentication, `pg_hba.conf`, the task per connection, cancel | `rupg-session`, `rupg-wire` | no | M2 | 8 |
| `rupg` | 13 | the Rust API, the facade crate that a user adds to `Cargo.toml` | `rupg-session`, optional `rupg-server` and `rupg-cluster` | no | M1 | 5 |
| `rupg-capi` | 13 | the C API and the libpq exports of document 17 | `rupg` | yes | M9 | 10 |
| `rupg-wasm` | 13 | the WebAssembly module and its storage on the OPFS | `rupg` | yes | M9 | 4 |
| `rupg-cli` | 13 | the `rupg` binary: `serve`, `check`, `strip`, `upgrade`, `import`, `dump` | `rupg`, `rupg-import` | no | M1 | 12 |
| `rupg-import` | 13 | the reader of PostgreSQL data directories and `pg_dump` archives | `rupg` | no | M9 | 20 |

### 22.4.1 Notes on some crates

**`rupg-file` is a format crate.** It reads and writes bytes in memory. It does not open a file and it does not do I/O. `rupg-buffer` and `rupg-log` do the I/O through `rupg-platform` and use `rupg-file` to read the bytes. A test can then build a page or a header slot in a byte array and check it with no disk. The `file_open` fuzz target of document 21 runs on this crate.

**`rupg-encode` does not decode into vectors on the scan path.** It parses the header of an encoded block and gives the parameters. `rupg-column` calls the kernels with those parameters. This keeps every hot loop in `rupg-kernels`, which is the one crate that has SIMD paths and `unsafe`.

**`rupg-tree` sees the buffer manager only through `PageAccess`.** The trait is in `rupg-file`. A test runs the tree on an in-memory implementation of the trait with no buffer manager. The hot store, the catalog and the B-tree index use the same tree.

**The mover is in `rupg-txn`.** Document 04 puts the mover in the table storage layer. The mover reads a hot row, writes it to a segment and changes the row's place under MVCC, so it needs the visibility rules. The code is therefore in `rupg-txn` at group 5, and it calls `rupg-table` to do the physical move. Document 24 has this as an open question.

**`rupg-shim` maps; it does not compute.** The catalog layer builds the version 19 answer. The shim changes the answer into the version that `rupg.compat_version` names. A shim that needs a different engine behavior is a defect in document 05, not a branch in the engine.

**`rupg-func` holds the regular expression engine.** PostgreSQL's regular expressions are Henry Spencer's advanced regular expressions, with rules that RE2 and the Rust `regex` crate do not share. rudb's `rudb-regex` follows DuckDB, which uses RE2, so rupg does not lift it.

**`rupg-session` holds the settings.** The 438 settings are generated from the vendored `guc_parameters.dat` by `cargo xtask codegen`, with their PostgreSQL names, types, units, ranges and defaults. The `rupg.*` settings are in a separate table in the same crate.

## 22.5 Outside dependencies

With the features of section 22.9 off, the `rupg` facade depends on `libc` and the four hash crates only. The table lists every outside crate that a shipped binary can link.

| Crate | Version | License | Used by | Feature | Why |
|---|---|---|---|---|---|
| `libc` | 0.2 | MIT OR Apache-2.0 | `rupg-platform` | always, on Unix | the system calls, `io_uring` set up, `madvise`, `fdatasync` |
| `rustls` | 0.23 | Apache-2.0 OR ISC OR MIT | `rupg-server` | `tls` | TLS for the server. rupg does not link OpenSSL |
| `ring` | 0.17 | Apache-2.0 AND ISC | `rustls` | `tls` | the crypto provider of rustls |
| `sha2`, `hmac`, `pbkdf2`, `md-5` | current | MIT OR Apache-2.0 | `rupg-wire`, `rupg-func` | always | SCRAM-SHA-256, MD5 passwords, and the `sha256()` and `md5()` functions |
| `cranelift-codegen`, `cranelift-frontend` | `=0.125.0` | Apache-2.0 WITH LLVM-exception | `rupg-jit` | `jit` | the second compile tier, pinned as in rudb because 0.125 is the last release that builds on the MSRV |
| `icu_collator` and its data | 2.x | Unicode-3.0 | `rupg-types` | `icu` | the ICU collation provider |
| `memchr` | 2.7 | Unlicense OR MIT | `rupg-kernels`, `rupg-func` | always | byte search in strings |
| `libm` | 0.2 | MIT | `rupg-func` | `wasm32` only | the math functions where no C library exists |
| `libmimalloc-sys` | 0.1, features `v2` and `no_thp` | MIT | `rupg-server`, `rupg-cli` binaries only | always, in those two binaries | rudb measured mimalloc 3 at twice the time of mimalloc 2 on its load, and transparent huge pages turned a 0.44 s load into 0.85 s (`rudb/Cargo.toml` at `cd9f9676`) |

The table has `sha2` and the others as always on, because SCRAM is part of every server build and the library needs `md5()` and `sha256()`. They are small and pure Rust.

The development dependencies are `proptest`, `loom` and `iced-x86` (for the tests of the direct compile backend, as in rudb). The `fuzz/` workspace adds `libfuzzer-sys` and `arbitrary`.

rupg does not use an async runtime. Document 04 section 4.5 gives one thread per core with rupg's own tasks, and `rupg-platform` implements the scheduler. The simulation of document 21 must control the scheduler, and an outside runtime would make that harder.

rupg does not use `chrono` or `chrono-tz`. PostgreSQL's date and time rules, including its input formats, its rounding and its time zone abbreviations, are not chrono's rules. `rupg-types` ports them from PostgreSQL, and the time zone data comes from the vendored IANA database.

rupg does not use `openraft` or another Raft crate. `rupg-raft` must run under the simulation with the platform traits, and it must write its log into the `.rupg` file. A crate with its own I/O and runtime cannot do either.

**The math functions are a compatibility risk.** PostgreSQL calls the C library for `sin`, `exp`, `pow` and the others, so the oracle's answers are glibc's answers on Linux. The Rust standard library also calls the platform's C library on Linux, so the answers match there. On macOS, Windows and wasm32 they can differ in the last bit. Document 24 has the question of whether rupg ships its own correctly rounded functions.

## 22.6 The license policy

`deny.toml` follows rudb's, with ISC added for `ring` and `rustls`.

| Section | Setting |
|---|---|
| `[licenses] allow` | Apache-2.0, Apache-2.0 WITH LLVM-exception, MIT, Unicode-3.0, ISC |
| `confidence-threshold` | 0.93 |
| `[advisories]` | `yanked = "deny"`, no ignored advisories |
| `[bans] wildcards` | deny |
| `[bans] multiple-versions` | warn |
| `[bans] deny` | `ahash`, and `hashbrown` except under `cranelift-codegen`, `regalloc2` and `indexmap` |
| `[sources]` | `unknown-registry = "deny"`, `unknown-git = "deny"`, crates.io only |

The allow list is a list of what is allowed, so every license not on it is denied. That includes the GPL family, the AGPL, the SSPL, the Business Source License and any license with a field of use limit. A dependency with a license that is not on the list needs a change to this document first.

The ban on two hashers is the rudb rule: two copies of a hasher in one process give two seeds, and a hash table built by one and probed by the other gives a wrong answer under a feature combination that no test ran.

The vendored files of section 22.8 are not crates, so `cargo-deny` does not see them. `cargo xtask vendor --check` checks that each vendored file has a license on a second list: the PostgreSQL License, public domain for the IANA time zone data, and Unicode-3.0 for the Unicode data. The PostgreSQL License is a permissive license similar to the BSD and MIT licenses, and Apache-2.0 can carry it.

## 22.7 What rupg lifts from rudb

A lift is a copy of a file from rudb into a rupg crate, with changes. rupg does not depend on a rudb crate (document 04 section 4.9). The table gives the planned lifts at rudb `cd9f9676`.

| From rudb | Into | Milestone | What changes |
|---|---|---|---|
| `crates/rudb-kernels/src/bits.rs`, `codec.rs`, `compare.rs`, `hash.rs`, `hashing.rs`, `aggregate.rs` and `aggregate/` | `rupg-kernels` | M1 to M4 | DuckDB types become PostgreSQL types. The hash function must give the same value as PostgreSQL's where `hashint4`, `hashtext` and the others are visible to a user |
| `crates/rudb-encoding/src/bitpack.rs`, `integer.rs`, `string.rs`, `sequence.rs`, `multi.rs`, `chooser.rs`, `reader.rs` | `rupg-encode` | M4 | the block headers become the format of document 09 |
| `crates/rudb-compress/src/zstd/`, `snappy.rs`, `gzip.rs` | `rupg-encode` | M4 | none planned. pglz and lz4 are new code, because TOAST needs them |
| `crates/rudb-storage/src/zone.rs`, `grams.rs`, `tally.rs` | `rupg-summary` | M4 | the summaries of document 09 section 9.7 and document 12 sections 12.6 and 12.9 |
| `crates/rudb-native/src/zones.rs`, `grams.rs`, `projection.rs`, `run_projection.rs`, `distinct.rs`, `postings.rs` | `rupg-column`, `rupg-summary` | M4 and M8 | the sorted projections of document 10 section 10.7 and the gram postings |
| `crates/rudb-graph/src/link.rs`, `adjacency.rs`, `rid.rs`, `rids.rs`, `keymap.rs`, `degree.rs`, `span.rs` | `rupg-graph` | M6 | the row ids become the rupg `RowId` with its shard field |
| `crates/rudb-qc-ir/src/*` | `rupg-jit` | M6 | the IR types gain the PostgreSQL types. The printer and parser are the text form of document 21 section 21.6 |
| `crates/rudb-qc-interp/src/lib.rs`, `crates/rudb-qc-clif/src/lib.rs`, `crates/rudb-qc-direct/src/` | `rupg-jit` | M6 | the tiers stay as they are, behind the `jit` feature for the Cranelift tier |
| `crates/rudb-qc-rt/src/abi.rs`, `code.rs`, `mem.rs`, `table.rs`, `like.rs` | `rupg-jit` | M6 | the runtime calls `rupg-func` in place of rudb's functions |

The frequency synopses of H3 come from the design in `../2140/storage-v3/11-certified-frequency-synopses.md`. If the rudb code for them is not on `main` at the time of M8, rupg writes them from the design.

Every lifted file starts with a provenance header:

```
// Lifted from rudb crates/rudb-encoding/src/bitpack.rs at cd9f9676 (2026-10-05).
// Changes: FOR base is i64 for int8; block header per 09-compression.md.
```

`cargo xtask lifts` reads every header, finds the file in a local rudb checkout, and reports each lifted file whose rudb source changed after the recorded commit. A rudb fix is then a decision to port or not to port, and not a change that nobody saw. rudb is Apache-2.0 and has the same author, so a lift needs no new license notice.

A port from PostgreSQL has the same header form with the PostgreSQL path and the pin:

```
// Ported from postgres src/backend/utils/adt/numeric.c at 7d3d2db7.
```

## 22.8 Vendored files and the xtask commands

The vendored files are in `vendor/`. Each directory has a `PIN` file with the source, the commit and the SHA-256 of each file.

| Vendored | From | Used by |
|---|---|---|
| `src/backend/parser/gram.y`, `src/include/parser/kwlist.h` | PostgreSQL pin | `rupg-sql` |
| `src/pl/plpgsql/src/pl_gram.y`, `pl_unreserved_kwlist.h`, `pl_reserved_kwlist.h` | PostgreSQL pin | `rupg-plpgsql` |
| `src/include/catalog/*.dat` | PostgreSQL pin | `rupg-pgcatalog` |
| `src/backend/utils/errcodes.txt` | PostgreSQL pin | `rupg-common`: the 268 SQLSTATEs |
| `src/backend/utils/misc/guc_parameters.dat` | PostgreSQL pin | `rupg-session`: the 438 settings |
| `src/backend/catalog/system_views.sql`, `information_schema.sql` | PostgreSQL pin | `rupg-pgcatalog`: the view definitions |
| the same files at 14.24, 15.19, 16.15, 17.11 and 18.6 | PostgreSQL tags | `rupg-shim`: the differences |
| `tzdata` | IANA, the release that the PostgreSQL pin uses | `rupg-types` |
| the Unicode character database | Unicode, the version that the PostgreSQL pin uses | `rupg-types`, `rupg-func` |

The generated code is checked in, so a build needs no generator and no network. CI runs each generator and fails if the output differs from the checked-in file.

| Command | What it does |
|---|---|
| `cargo xtask layers` | checks the ranks of section 22.3 |
| `cargo xtask style` | checks `forbid(unsafe_code)`, `SAFETY` comments, provenance headers, line length, and the writing rules of document 00 in comments |
| `cargo xtask vendor [--check]` | fetches the vendored files at their pins and checks the hashes and the licenses |
| `cargo xtask grammar` | runs the LALR(1) generator on `gram.y` and `pl_gram.y` and writes the tables into `rupg-sql` and `rupg-plpgsql` |
| `cargo xtask codegen` | builds the built-in catalog from the `.dat` files, the settings table and the SQLSTATE table |
| `cargo xtask unicode`, `cargo xtask tz` | builds the Unicode and time zone tables |
| `cargo xtask lifts` | section 22.7 |
| `cargo xtask bless` | rewrites the expected text forms (document 21 section 21.6) |
| `cargo xtask bench` | runs the instruction count set of document 21 section 21.14 |
| `cargo xtask version` | sets the workspace version and every exact internal pin |

The LALR(1) generator is rupg's own code in `xtask`. It reads the bison grammar with its precedence declarations and must report the same conflicts that bison reports on the same file. PostgreSQL's `gram.y` has `%expect 0`, so the check is that the generator also finds no conflict.

## 22.9 Features

The features are on the facade crate `rupg` and are passed down.

| Feature | Default | Turns on |
|---|---|---|
| `jit` | yes, except on wasm32 | the Cranelift tier in `rupg-jit`. The interpreter tier and the direct x86-64 tier are always present on their targets |
| `icu` | yes | the ICU collation provider |
| `server` | no | `rupg-server`, and with it `tls` |
| `tls` | no | `rustls` and `ring` |
| `cluster` | no | `rupg-raft` and `rupg-cluster` |
| `contrib` | yes | `rupg-contrib` |
| `import` | no | `rupg-import` |
| `sim` | no | the simulation implementations of the platform traits, for tests only |

The `rupg` binary of `rupg-cli` turns on `server`, `cluster` and `import`. A library user who adds `rupg` to `Cargo.toml` gets the engine with the compile tiers, ICU collations and contrib, and no network code. Gate measurements use the `rupg` binary.

The property graph syntax of document 13 is off by default. It is a setting, not a feature, because the grammar is one grammar.

## 22.10 Build profiles

The profiles follow rudb's root `Cargo.toml`.

| Profile | Settings | Why |
|---|---|---|
| `release` | `lto = "thin"`, `codegen-units = 1`, `strip = "debuginfo"`, unwinding on | the C API catches a panic at the boundary and returns an error code, which needs an unwinder |
| `profiling` | `release` with `debug = 1`, `strip = "none"` | a database is profiled more often than it is shipped |
| `bench` | `profiling` | the numbers of document 21 section 21.14 come from the build that a developer profiles |
| `dev` | `opt-level = 2` for dependencies only | the tests run at benchmark scale and stay debuggable |

rudb found that `opt-level = "z"` on the CLI crate, with LTO, size-optimized the whole engine and made it slower: 6.9 percent more instructions over TPC-H and 25 percent more cycles on Q1 (`rudb/Cargo.toml` at `cd9f9676`). rupg does not set a size optimization on any crate for that reason. Fat LTO and profile-guided optimization are not on until a measurement in `rupg-bench` shows a gain.

## 22.11 The toolchain

The toolchain is pinned to Rust 1.98.0 in `rust-toolchain.toml`, with `rustfmt`, `clippy` and `rust-src`. A new clippy or rustfmt release then cannot turn CI red overnight. A change to the pin is one commit.

The MSRV is 1.88.0, as in rudb, and the CI job of document 21 section 21.16 builds on it. Two features set the floor. `#[target_feature]` on a safe function became stable in 1.86, and the kernels use it for each SIMD path. `core::hint::select_unpredictable` became stable in 1.88, and the filter kernels use it for branch-free selection. The edition is 2024 and the resolver is 3. The license of every rupg crate is Apache-2.0.

## 22.12 The sibling repositories

| Repository | Holds | Depends on rupg through |
|---|---|---|
| `github.com/tamnd/rupg` | the engine, this layout | |
| `github.com/tamnd/rupg-compat` | the oracle, the harness and the suites of document 21 section 21.3 | a socket, the C API, the libpq exports |
| `github.com/tamnd/rupg-bench` | the benchmark harness, the machine scripts, the reports and the performance ratchet | the `rupg` binary and the facade crate at a commit |

On 7 October 2026, `gh repo view` returned "Could not resolve to a Repository" for `tamnd/rupg-compat` and `tamnd/rupg-bench`, so both names are free.

`rupg-compat` does not depend on a rupg crate (document 21 section 21.3.1). `rupg-bench` depends on the facade crate for the in-process transport and on the binary for the socket transport, because document 02 requires the same transport for rupg and for each baseline.

## 22.13 Crate names

On 7 October 2026 the crates.io API returned 404 for all 41 names of section 22.4: `rupg` and the 40 `rupg-*` names. All 41 are free. The crates.io policy does not allow name squatting, so rupg publishes the crates when the first milestone with a public API ships (M1 for `rupg`, document 23) and not before. The names can be taken before then. If one is taken, the crate gets a new name, and the change is one line in each of this document, `Cargo.toml` and `layers.toml`.

## 22.14 Line budgets

The budgets of section 22.4 add up to 913,000 lines. rudb has 421,288 lines in `crates/*/src` at `cd9f9676`, counted with `find crates -name '*.rs' -path '*/src/*' | xargs cat | wc -l`. That count includes comments and blank lines, and the rupg budgets use the same count. The generated tables of section 22.8 are not in the budgets.

The three largest budgets are `rupg-func` at 120,000 lines, `rupg-exec` at 60,000 and `rupg-contrib` at 50,000. `rupg-func` is large because PostgreSQL has 3,414 built-in functions at the pin and each must give PostgreSQL's answer and error for every input. `rupg-contrib` is large because document 16 ports the contrib extensions to Rust and does not load C code.

A budget is a planning number. It is not a limit and it is not a target. When a crate passes its budget by 50 percent, the milestone report says why, and the reviewer decides whether the crate should split. A split adds a crate, and section 22.1 says that a new crate is a design decision.
