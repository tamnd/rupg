# Spec 2140pg: rupg

rupg is a PostgreSQL server and a PostgreSQL library in one Rust program. A client that connects to PostgreSQL 19 connects to rupg with no change except the host, the port and the credentials. The same engine also runs inside the application process, on one file, with no server. The data is stored in that one file, as compressed columns for cold data and as row pages for hot data. The file format, the transaction timestamps and the log are designed so that a database can later be split into shards and replicated across machines with no change to the format.

The performance requirement is hard and it is stated in numbers. On ClickBench, rupg must be 10x faster than PostgreSQL, ClickHouse and DuckDB, and it must use 10x fewer resources, on the same machine. TPC-H, TPC-C and YCSB follow with their own gates. Document 02 states each number, shows the arithmetic, and says which parts of the requirement the published data supports and which part it does not.

Written 7 October 2026. The pins are PostgreSQL `REL_19_STABLE` at `7d3d2db7` (stamped 19beta4), the ClickBench repository at `e6bda4e` (committed 6 October 2026), and rudb at `cd9f9676` (`v0.8.23-1`). Every count in this folder says where it came from. A count without a source is a mistake. You must fix it or delete it.

Repo: `github.com/tamnd/rupg`. Sibling repos: `github.com/tamnd/rupg-compat` for the compatibility oracle and its suites, and `github.com/tamnd/rupg-bench` for measurement. On 7 October 2026, `gh repo view tamnd/rupg` returned "Could not resolve to a Repository", so the name is free on GitHub. The crate names in document 22 must be checked on crates.io before the first publish.

## What rupg is

rupg is one engine with four ways in.

1. **The server.** rupg listens on a TCP port and a Unix socket and speaks the PostgreSQL frontend and backend protocol, versions 3.0 and 3.2. This is the way in for every existing driver, ORM, pooler, GUI and BI tool.
2. **The library.** A Rust crate, a C library and a WebAssembly module open a `.rupg` file in process. The C library also exports the libpq functions, so a program that links libpq can open a file with no server.
3. **The replica.** A rupg server follows another rupg server through the log, and later through a Raft group per shard.
4. **The importer.** rupg reads a stopped PostgreSQL 14 to 19 data directory and a `pg_dump` archive, and it consumes the logical replication stream of a running PostgreSQL server.

All four ways use the same parser, the same catalog, the same executor and the same file format. No behavior depends on the way in, except the transport and the authentication.

## Why a new engine and not rudb with a PostgreSQL front door

`../2140/compat/postgres/` specifies a PostgreSQL front door for rudb. That plan is sound for its goal, and rupg takes most of its surface work from it: the levels of compatibility, the vendored grammar, the exact OIDs, the virtual catalog, the oracle harness and the performance arithmetic. rupg is a separate engine because three of its requirements conflict with settled decisions of rudb.

**rudb is single node by decision.** `../2140/00-README.md` says "No distributed execution" and gives the Cloudspecs argument. rupg must be ready for shards and replicas. That changes the transaction timestamps, the row identifiers, the log format and the catalog versioning, and those choices are expensive to change after the file format ships.

**rudb is DuckDB first.** Its dialect, its types, its storage format compatibility and its extension ABI are DuckDB's. In rupg the PostgreSQL semantics are the only semantics. There is no dialect seam, so no pass below the parser carries a second set of rules for `numeric`, `timestamptz`, collations, `NULL` ordering or integer overflow.

**rudb is analytic first.** Its point path was added at W4 (`v0.8.23`), and it has no B-tree (`../2140/compat/postgres/03-where-rudb-is.md`). rupg must run TPC-C and YCSB at 10x the CPU efficiency of PostgreSQL, so the row store, the buffer manager, the index structures and the log are first-class from the first milestone, not an addition.

rupg does not depend on rudb crates at run time. It lifts code from rudb where rudb has a measured result, for example the FastLanes kernels, the global string codes, the zone maps and the link graph. Each lifted file records the rudb commit that it came from. After the lift, the two copies diverge. Document 22 lists every lift, and document 24 keeps the question of a shared kernel crate open.

## The situation as of today

PostgreSQL 19 is at beta 4, released on 24 September 2026. RC1 is planned for 15 October and GA for 29 October 2026. Before beta 4 the release team removed SQL/PGQ, `FOR PORTION OF`, `MERGE PARTITIONS`, `SPLIT PARTITIONS` and online checksum changes. So SQL/PGQ is not a PostgreSQL 19 feature, and document 13 treats property graph syntax as a rupg addition that is off by default. PostgreSQL 14 reaches end of life on 12 November 2026, and the supported set after 19.0 is 15 to 19.

On ClickBench at `c6a.4xlarge`, the sums of the best hot run over 43 queries are 11,875.43 s for PostgreSQL, 26.25 s for DuckDB, 17.53 s for ClickHouse, 7.41 s for Umbra and 5.21 s for elosdb, which is first on the board. These numbers come from the result files at the pinned commit. Document 03 has the full table.

Four systems are near rupg's goal and each one shows a different part of it. pgrust is a clean-slate Rust rewrite of PostgreSQL with threads and a vectorized executor. Its authors report that v0.1 passed all 46,066 regression queries of PostgreSQL 18.3, and its ClickBench hot sum is 34.88 s. CedarDB is Umbra sold as a PostgreSQL-compatible server, with a hot sum of 28.99 s. PGlite is PostgreSQL compiled to WebAssembly as a single-connection library. Aurora DSQL is a sharded PostgreSQL-compatible service with optimistic concurrency and commit-time coordination, and its design paper (arXiv 2607.13276) gives the latency numbers that document 18 compares with.

No system meets all four parts of the goal. pgrust and CedarDB are not embedded and do not shard. PGlite is embedded but it is the PostgreSQL row engine with one connection. DSQL is not embedded, not columnar, and supports only snapshot isolation. Document 01 gives the evidence for each claim.

## The goal

Document 02 states the goal as five axes. Each axis has a number, a machine, a baseline and a gate.

**Compatibility.** rupg passes the PostgreSQL 19 regression suite, the isolation suite, and the test suites of 48 clients, at the same rate as a real PostgreSQL 19 server, with every difference listed. Older servers from 14 to 18 are emulated by a version shim. Document 05.

**Analytic speed.** On ClickBench at `c6a.4xlarge`, the hot sum is at most 1.75 s, which is one tenth of ClickHouse's 17.53 s and less than one tenth of DuckDB and PostgreSQL. TPC-H at SF100 follows at 10x DuckDB. Document 02 shows that 1.75 s is 4.2x past Umbra and 3.0x past the first system on the board, and it says what has to be true for it to hold.

**Transactional speed.** On TPC-C and YCSB, server CPU per transaction is at most one tenth of PostgreSQL's on the same machine. The latency at one client is not part of the 10x claim, because the kernel and the client set a floor under every statement (`../2140/compat/postgres/15-performance.md` section 15.2).

**Resource.** Peak memory and total CPU-seconds are at most one tenth of each baseline on the same suite. Bytes on disk are at most one tenth of PostgreSQL's. Against ClickHouse and DuckDB the disk target is smaller than both but not 10x, because the measured content of the `hits` table does not allow it. Document 02 section 2.6 gives the measurement.

**Scale out.** A cluster of N nodes runs TPC-C with throughput that grows at least 0.8 N times the single node throughput up to 16 nodes, with serializable or snapshot isolation as the user selects. Document 18.

## Settled decisions

These decisions are fixed. To change one, you must first write the measurement that shows it is wrong in document 24.

**PostgreSQL is the only dialect.** There is one grammar, one set of type rules and one set of function semantics. rupg does not accept DuckDB syntax and does not have a dialect setting. Document 07.

**Target PostgreSQL 19.0, with shims for 14 to 18.** By default the server reports `server_version` as `19.0` and `server_version_num` as `190000`. The setting `rupg.compat_version` selects an older major version per database or per role. The shim changes the version strings, the catalog columns that a client can see, the configuration parameters that exist, and the small set of behaviors that changed between versions. It does not change the engine. Document 05.

**Built-in objects use the exact PostgreSQL OIDs.** The catalog data files `pg_proc.dat`, `pg_type.dat`, `pg_operator.dat`, `pg_cast.dat` and the others are vendored from the pin, and a build step turns them into static tables. User objects get OIDs from 16384 upward. Document 07.

**The grammar is vendored from `gram.y`.** A build step generates LALR(1) tables from the rules of the pinned `gram.y`. The semantic actions are ours. The build fails on any grammar conflict. Document 07.

**One database is one file.** The file has the extension `.rupg`. The log, the catalog, the row pages, the column segments, the indexes and the free space map are all inside it. There is no sidecar file at rest and none during operation. Document 08.

**One process owns a file.** The first process that opens a file for writing becomes its owner. A second process that opens the same file connects to the owner through a local socket, and it sees the same transactions. This is how one file serves many processes without shared memory files. Document 17.

**Hot rows and cold columns live in the same table.** New and recently changed rows go into a B+tree of row pages. A background task moves stable rows into immutable compressed column segments. A query reads both and sees one table. The user does not choose the layout. `CREATE TABLE ... USING columnar` and storage parameters are hints, not modes. Document 10.

**MVCC uses in-place updates and undo, not a heap of tuple versions.** A row has one current version in storage. Older versions are in per-transaction undo records. Old versions are removed without waiting for the oldest snapshot, as in the scalable snapshot isolation design of Alhomssi and Leis (PVLDB 16, 2023). `VACUUM` is accepted and does compaction, but no workload needs it for correctness or for speed. The system columns `ctid`, `xmin`, `xmax`, `cmin`, `cmax` and `tableoid` are computed so that clients that read them, for example Entity Framework with `xmin` as a concurrency token, keep working. Document 11.

**The log has one ring per worker, inside the file.** Each worker thread writes its own log records. A commit is acknowledged when the log records it depends on are durable, without one global flush order. Pages are written out of place. This follows Nguyen, Alhomssi, Ziegler and Leis (SIGMOD 2025) and Lee, Ziegler and Leis (PVLDB 19, 2026). Document 11.

**Every timestamp is a hybrid logical clock value from the first commit.** A single-node database uses the same 64-bit commit timestamps that a cluster uses. Row identifiers carry a shard number that is zero on a single node. Log records name the shard that they change. These three choices are why the format is ready for a cluster. Document 18.

**Execution is vectorized in units of 1024 values and works on compressed data.** 1024 is the FastLanes unit, so the compression layer and the executor share one granularity. A dictionary-coded column is grouped on its codes and a frame-of-reference column is filtered by a changed predicate. Short statements use a point path with no plan tree. Long pipelines are compiled. Document 14.

**Joins are made robust in the executor, not in the estimator.** The executor uses predicate transfer and exact semi-join reduction, and unchained hash tables (Birler, Schmidt, Fent and Neumann, DaMoN 2024). The optimizer has no learned component. Document 15.

**Graph indexes are first-class access methods.** `USING link` stores the adjacency between the rows of two tables, the way rudb's link graph does, as a CSR structure inside the file. It speeds up joins, recursive queries and graph traversal. `USING vector` stores a proximity graph for nearest neighbor search, with RaBitQ codes. The pgvector syntax `USING hnsw` and `USING ivfflat` is accepted and maps to it. Document 13.

**A session is a task, not a process.** The server runs one thread per core. Each thread runs many sessions as tasks and uses `io_uring` on Linux and `kqueue` on macOS. An idle session costs at most 64 KiB. PostgreSQL forks one process for each connection, and that is the largest single cost on the resource axis for many small connections. Document 06.

**The library is the engine.** The server is a thin crate over the library. Anything the server can do, the library can do in process. Document 17.

**rupg does not load PostgreSQL C extensions.** A PostgreSQL extension calls the internal C functions of the server, and rupg does not have them. rupg implements the most used extensions natively, starting with pgvector, `pg_trgm`, `pgcrypto`, `uuid-ossp`, `hstore`, `citext`, `ltree`, `btree_gin`, `btree_gist`, `pg_stat_statements` and `postgres_fdw`. New extensions use a Rust API or a WebAssembly API. PL/pgSQL is built in. Document 16.

**Clean room for copyleft code.** You may read and vendor PostgreSQL source, which is under the PostgreSQL License. You must not read the source of pgrust, ParadeDB, PgDog, Hydra or any other AGPL or GPL database. You may cite their published numbers as facts about them. You must never use their published numbers as a baseline. A baseline is a number that we measured.

**Compatibility is measured by an oracle, never asserted.** `rupg-compat` runs a real PostgreSQL 19 server built from the pin and rupg side by side. It sends the same input to both and compares every byte that a client can see. Document 21.

**Performance is measured per suite, per machine and per transport.** A rupg number is compared only with a baseline number that we measured on the same machine through the same transport. An in-process number is never compared with a number measured over a socket. Document 20.

**No GPU in the core.** GPU engines hold ClickBench cost records. Sirius (CIDR 2026) reports 7.4x better cost efficiency than DuckDB on ClickBench. rupg is a CPU engine because it must run embedded on a laptop and in WebAssembly. The exchange boundary of document 14 keeps a GPU offload crate possible.

**Toolchain.** Stable Rust, edition 2024, pinned to the same toolchain as rudb (`1.98.0` at `cd9f9676`). License Apache-2.0. `unsafe` code is allowed only in the crates that document 22 lists, and each block has a `SAFETY` comment.

## The documents

| | | |
|---|---|---|
| 00 | `00-README.md` | this file: the pitch, the settled decisions, the vocabulary |
| 01 | `01-research-2026.md` | the systems and papers as of October 2026, and what each one forces |
| 02 | `02-the-goal.md` | the five axes, the arithmetic of 10x, and what the data supports |
| 03 | `03-baselines.md` | measured numbers for PostgreSQL, ClickHouse, DuckDB and others, and where their time goes |
| 04 | `04-architecture.md` | the layers, the threads, the data flow and the rules between crates |
| 05 | `05-compatibility.md` | what 100 percent means, the PostgreSQL 19 surface, the version shim, and what we do not count |
| 06 | `06-wire-and-sessions.md` | the protocol, the server runtime, authentication, sessions and settings |
| 07 | `07-sql-types-and-catalog.md` | the vendored grammar, the analyzer, the types, the OIDs and the virtual catalog |
| 08 | `08-the-file.md` | the single file format: header slots, pages, extents, checksums, free space and atomic commit |
| 09 | `09-compression.md` | the encodings, the cascades, the encoder, and the global string codes |
| 10 | `10-rows-and-columns.md` | the hot row store, the cold column segments, the move between them, TOAST and system columns |
| 11 | `11-transactions-and-log.md` | MVCC, isolation levels, SSI, locks, the per-worker log, checkpoints and recovery |
| 12 | `12-indexes.md` | B-tree, hash, GIN, GiST, SP-GiST and BRIN on rupg storage, zone maps, projections and synopses |
| 13 | `13-graph-indexes.md` | the link graph, CSR adjacency, recursive and graph queries, and the vector graph |
| 14 | `14-execution.md` | the vectorized engine, compressed execution, joins, aggregation, the point path, compile tiers and memory |
| 15 | `15-optimizer.md` | the planner, statistics, robust joins, `EXPLAIN` compatibility and the planner settings |
| 16 | `16-procedures-and-extensions.md` | PL/pgSQL, triggers, the built-in extensions, and the extension API |
| 17 | `17-embedding.md` | the Rust, C, libpq and WebAssembly interfaces, file ownership, and many processes on one file |
| 18 | `18-cluster.md` | shards, replicas, distributed transactions, distributed queries, and rebalancing |
| 19 | `19-resources.md` | the memory, CPU and disk budgets, and how each one is measured |
| 20 | `20-benchmarks.md` | ClickBench, TPC-H, TPC-C and YCSB: machines, rules, transports and gates |
| 21 | `21-testing.md` | the oracle, the suites, fuzzing, crash tests and deterministic simulation |
| 22 | `22-crate-layout.md` | the crates, their layer ranks, the lifts from rudb, and the dependency rules |
| 23 | `23-milestones.md` | milestones M0 to M12, the gate of each, and why the order is what it is |
| 24 | `24-open-questions.md` | the decisions that a measurement has not made yet |

## How to read this if you are short of time

Read document 02, document 04 and document 23. Document 02 says what the 10x requirement means and where it is hard. Document 04 shows how the parts fit. Document 23 says what to build first.

The most important fact in this folder is in document 02. On ClickBench, 10x ClickHouse is 1.75 s on a machine where the best published system takes 5.21 s. No engine reaches that number by being a better PostgreSQL. It needs a storage layer that answers most of each query from small, exact, reusable summaries of the data, and an executor that never decodes a value it does not need. That is why documents 08, 09, 10 and 12 are long.

## Vocabulary

**The pin.** PostgreSQL `REL_19_STABLE` at `7d3d2db7`. When 19.0 is tagged on or after 29 October 2026, the pin moves to the tag in one change, and every count in this folder is taken again.

**The oracle.** A PostgreSQL 19 server built from the pin, run by `rupg-compat` with a fixed configuration. Its answer is correct by definition, including where it disagrees with the documentation.

**The file.** One `.rupg` file. It holds one PostgreSQL database cluster: its databases, roles, tablespaces as names only, and all their data.

**Hot store.** The B+tree of row pages that holds new and recently changed rows of a table.

**Cold store.** The immutable compressed column segments that hold stable rows of a table.

**Segment.** A unit of the cold store. It holds up to 262,144 rows of one table, as one column chunk per column.

**Vector.** 1024 consecutive values of one column. The unit of encoding, of zone maps and of execution.

**Worker.** One operating system thread, pinned to one core in server mode. A worker runs session tasks, query tasks and background tasks.

**Shard.** A set of rows of one or more tables that move together between nodes. On a single node there is one shard, number 0.

**Link.** A stored mapping from the rows of one table to the rows of another, for example from each `lineitem` row to its `orders` row. The term comes from `../2140/graph/`.

**A difference.** Any byte that a client can see and that differs between rupg and the oracle for the same input. Document 05 lists the bytes we exclude.

## Rules for this folder

Every number has a source: a file in a repository at a commit, a paper with its venue, or a measurement with its machine and date. A prediction is written as a prediction. When a measurement replaces it, the prediction stays in the text and the measurement is added after it, with the date.

The writing follows ASD-STE100 where a technical rule is stated. Use short sentences, the active voice, one instruction in each sentence, and "must" for a requirement. Write each paragraph as one line. Do not use dashes as punctuation.
