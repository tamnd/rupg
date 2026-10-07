# rupg

A PostgreSQL 19 compatible database in Rust. It runs as a server and as an embedded library, on one file.

[![ci](https://github.com/tamnd/rupg/actions/workflows/ci.yml/badge.svg)](https://github.com/tamnd/rupg/actions/workflows/ci.yml) [![security](https://github.com/tamnd/rupg/actions/workflows/security.yml/badge.svg)](https://github.com/tamnd/rupg/actions/workflows/security.yml) [![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE-APACHE)

A client that connects to PostgreSQL 19 connects to rupg with no change except the host, the port and the credentials. The same engine also runs inside the application process, with no server. A database is one `.rupg` file. Cold data is in compressed columns. Hot data is in row pages. The file format, the timestamps and the log are designed for shards and replicas from the start.

**Status: M0.** This repository has the workspace, the layer rule, the CI and the full specification. It does not store data or answer queries yet. The milestones that build the engine are issues with the label [`kind/milestone`](https://github.com/tamnd/rupg/issues?q=label%3Akind%2Fmilestone).

## The goal

The goal has five axes. Each axis has a number, a machine, a baseline and a gate. [`spec/02-the-goal.md`](spec/02-the-goal.md) gives the arithmetic for each number and says what must be true for it to hold.

| Axis | Target | Spec |
|---|---|---|
| Compatibility | Pass the PostgreSQL 19 regression suite, the isolation suite and the suites of 48 clients at the rate of a real PostgreSQL 19 server. Shims emulate versions 14 to 18. | [05](spec/05-compatibility.md) |
| Analytic speed | ClickBench hot sum at most 1.753 s on `c6a.4xlarge`. That is one tenth of ClickHouse (17.53 s). TPC-H SF100 at 10x DuckDB. | [02](spec/02-the-goal.md) |
| Transactional speed | TPC-C and YCSB at one tenth of the server CPU per transaction of PostgreSQL, on the same machine. | [02](spec/02-the-goal.md) |
| Resources | One tenth of the peak memory and CPU time of each baseline. Disk below 7.67 GB on ClickBench. | [19](spec/19-resources.md) |
| Scale out | TPC-C on 2 to 16 nodes at 0.8 N or more of one node. | [18](spec/18-cluster.md) |

A 10x claim has five parts: one machine, one metric, a baseline that we measured on that machine, the same transport for both systems, and the ratio. A claim without all five parts is not made. The measurements are in [rupg-bench](https://github.com/tamnd/rupg-bench). The compatibility levels are in [rupg-compat](https://github.com/tamnd/rupg-compat).

## Gates

Each gate is a measured number that decides if a milestone is done. When a gate fails, we publish the number, the gap and the cause. We do not change the target in the same change.

| Gate | Suite | Number | Milestone |
|---|---|---|---|
| G1 | L1 and L2 | 7 clients connect and show the schema: psql, pgAdmin 4, DBeaver, psycopg 3, node-postgres, pgx, asyncpg | M2 |
| G2 | ClickBench, `c6a.4xlarge` | hot sum below ClickHouse (17.53 s) | M4 |
| G3 | ClickBench, `c6a.4xlarge` | disk below 9.45 GB | M4 |
| G4 | YCSB A, B, C, F | 3x server CPU per operation, no pipelining | M3 |
| G5 | TPC-C, statements | 3x NOPM per core | M5 |
| G6 | TPC-C, procedures | 10x NOPM per core | M7 |
| G7 | ClickBench, `c6a.4xlarge` | hot sum at most 1.753 s, cold sum at most 11.37 s | M8 |
| G8 | ClickBench, `c6a.4xlarge` | 10x peak memory and CPU time, disk below 7.67 GB | M8 |
| G9 | TPC-H SF100 | 10x DuckDB hot total | M8 |
| G10 | ClickBench, `c6a.metal` | hot sum at most 0.440 s | M8 |
| G11 | TPC-C, 2 to 16 nodes | at least 0.8 N | M11 |
| G12 | L1 to L5, PostgreSQL 19 and shims 14 to 18 | 100 percent | M12 |

## The design on one page

- **One engine, four ways in.** The server speaks protocol 3.0 and 3.2. The library opens a file in process from Rust, C (with libpq exports) or WebAssembly. A replica follows the log. The importer reads PostgreSQL data directories, `pg_dump` archives and logical replication. All four use the same parser, catalog, executor and file.
- **One file.** A `.rupg` file has no sidecar. Hot rows are in a B+ tree of PAX pages. Cold rows are in column segments of up to 262,144 rows with global dictionaries and per-segment summaries. Writes go out of place, so a torn page cannot damage committed data. ([spec/08](spec/08-the-file.md), [spec/09](spec/09-compression.md), [spec/10](spec/10-rows-and-columns.md))
- **Transactions.** MVCC with undo, hybrid logical clock timestamps (48 bits of milliseconds and 16 logical bits), SSI, and one log ring per worker. A row id has a 16-bit shard and 48 local bits, so a later split does not rewrite rows. ([spec/11](spec/11-transactions-and-log.md))
- **Execution.** Thread per core with io_uring on Linux. Vectors of 1024 values. Operators run on the encoded data. Three compile tiers: an interpreter, a direct x86-64 tier and Cranelift. ([spec/14](spec/14-execution.md))
- **Indexes.** All PostgreSQL access methods, plus `USING link` for graph adjacency and `USING vector` for DiskANN with RaBitQ, which `hnsw` and `ivfflat` map to. ([spec/12](spec/12-indexes.md), [spec/13](spec/13-graph-indexes.md))
- **Extensions.** Built-in extensions are Rust code. There is no C extension ABI. ([spec/16](spec/16-procedures-and-extensions.md))
- **Cluster.** One Raft group per shard. A table is split into shards by a hash of its distribution key, as in PostgreSQL hash partitioning. An unchanged schema runs on shard 0. Distributed transactions commit at an HLC timestamp. ([spec/18](spec/18-cluster.md))

## Building

You need Rust 1.98.0. The `rust-toolchain.toml` file selects it. The MSRV is 1.88.0.

```sh
cargo build --release
./target/release/rupg --version
./target/release/rupg --print-config
cargo test --workspace
```

Run the same checks as CI before you push:

```sh
cargo fmt --all --check
cargo xtask layers
cargo xtask style
cargo xtask vendor --check
cargo xtask grammar
cargo clippy --workspace --all-targets --all-features
```

## Repository layout

| Path | Contents |
|---|---|
| `crates/` | The 41 crates of [spec/22](spec/22-crate-layout.md). Most are stubs until their milestone. |
| `xtask/` | The check tasks. `layers.toml` has the rank of each crate. A crate may depend only on a lower rank. |
| `vendor/` | Files from PostgreSQL, the time zone data and Unicode, at their pins. Each directory has a `PIN` file with the hashes. |
| `spec/` | The technical specification, 25 documents. |
| `.github/` | CI, the issue templates and Dependabot. |

## The specification

The design is written before it is built. Start with [`spec/00-README.md`](spec/00-README.md). The milestones are in [`spec/23-milestones.md`](spec/23-milestones.md). The open questions are in [`spec/24-open-questions.md`](spec/24-open-questions.md). A path that starts with `../2140/` names a file in the [rudb specification](https://github.com/tamnd/rudb/tree/main/spec).

## Sibling repositories

| Repository | Purpose |
|---|---|
| [tamnd/rupg-compat](https://github.com/tamnd/rupg-compat) | The PostgreSQL oracles from 14 to 19, the runner and the suites that decide the compatibility levels L1 to L5. |
| [tamnd/rupg-bench](https://github.com/tamnd/rupg-bench) | The benchmark harness, the baselines and the reports that decide the performance gates. |
| [tamnd/rudb](https://github.com/tamnd/rudb) | The analytical engine that rupg lifts kernels and encodings from. |

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md). To report a wrong answer, use the wrong answer template. A wrong answer is the most serious kind of bug. To report a security problem, read [SECURITY.md](SECURITY.md).

## License

Apache-2.0. See [LICENSE-APACHE](LICENSE-APACHE). rupg is a clean room implementation. Do not read or copy code under the AGPL or another copyleft license into this repository.
