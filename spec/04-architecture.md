# 4. Architecture

Written 7 October 2026.

This document shows how the parts of rupg fit together. It names the layers, follows a query and a write through them, gives the thread model, fixes the identifiers that every layer shares, and states the rules between layers. The documents 06 to 18 specify each layer. Document 22 gives the crates and their ranks.

## 4.1 The layers

rupg has thirteen layers. A layer may call a layer below it. A layer must not call a layer above it. The cluster layer is the one exception to a strict stack: it sits beside the transaction and storage layers and it is called by them through traits that the lower layer defines. The table goes from the top to the bottom.

| Rank | Layer | What it owns | Document |
|---|---|---|---|
| 13 | Front ends | the wire server, the Rust API, the C API with the libpq exports, the WebAssembly module, the CLI | 06, 17 |
| 12 | Sessions | session state, settings, portals, prepared statements, the plan cache, `LISTEN` and `NOTIFY` | 06 |
| 11 | Procedures | PL/pgSQL, triggers, rules, the extension host | 16 |
| 10 | Execution | the vectorized engine, the point path, the compile tiers, the exchange operators | 14 |
| 9 | Planning | the analyzer, the rewriter, the optimizer, statistics, `EXPLAIN` | 07, 15 |
| 8 | SQL front | the lexer, the vendored grammar, the parse tree | 07 |
| 7 | Catalog | the rupg catalog, the virtual `pg_catalog` and `information_schema`, the version shim | 05, 07 |
| 6 | Access methods | B-tree, hash, GIN, GiST, SP-GiST, BRIN, link, vector, and the summaries | 12, 13 |
| 5 | Transactions | MVCC, undo, snapshots, locks, SSI, commit timestamps, the mover policy | 11, 10 |
| 4 | Table storage | the hot store, the cold store, the physical move of rows, TOAST, the system columns | 10 |
| 3 | Log | the per-worker log rings, checkpoints, recovery, the replication stream | 11 |
| 2 | Pages | the buffer manager, the page and extent formats, encodings and their kernels | 08, 09 |
| 1 | File and platform | the file, the header slots, free space, checksums, I/O, the clock, memory accounting | 08 |
| beside 3 to 5 | Cluster | placement, Raft groups, distributed commit, the shard map, rebalancing | 18 |

Types and the value representation are in a common crate at rank 0, which every layer may use. The PostgreSQL types, their text and binary forms and their operators are in rank 0 and rank 7 crates. Document 22 has the split.

## 4.2 A query from the socket to the socket

This section follows `SELECT RegionID, count(*) FROM hits WHERE AdvEngineID <> 0 GROUP BY 1` sent with the extended protocol on a TCP connection.

1. **The worker reads bytes.** The session task of the connection is on a worker thread. The worker's I/O ring completes a receive into the session's input buffer. The protocol codec, which has no I/O, turns the bytes into `Parse`, `Bind`, `Describe`, `Execute` and `Sync` messages. Document 06.
2. **The session looks in the plan cache.** The key is the query text, the parameter types, the search path and the catalog version. A hit returns a prepared plan and skips steps 3 to 5.
3. **The parser builds a parse tree.** The lexer and the LALR(1) tables generated from `gram.y` produce a tree with the same node kinds as PostgreSQL's raw parse tree. Document 07.
4. **The analyzer binds names and types.** It reads the catalog through a snapshot, resolves `hits`, `RegionID` and `AdvEngineID`, and applies PostgreSQL's rules for operator and function resolution. The result is a query tree with OIDs. Rules and views are expanded here. Document 07.
5. **The optimizer builds a physical plan.** It reads table statistics, the segment summaries and the available indexes. For this query it chooses a scan of `hits` that filters on the codes of `AdvEngineID`, skips every segment whose summary shows only zeros, and groups on the codes of `RegionID` in a small dense array. Document 15.
6. **The executor splits the plan into pipelines.** Each pipeline runs on morsels of segments and of hot store pages. The session task submits the morsels to the query scheduler of the node. Workers pull morsels, run the pipeline on vectors of 1024 values, and merge their partial groups at the pipeline break. Document 14.
7. **The encoder writes `DataRow` messages.** It writes the result vectors into the session's output buffer one column at a time, in text or binary format as the `Bind` asked. It writes `CommandComplete` and `ReadyForQuery`. The worker submits a send on its I/O ring. Document 06.

In the library the steps are the same except 1 and 7. The caller passes the text and the parameters as Rust values or C values, and it reads the result vectors directly. Nothing in steps 2 to 6 knows which way the query came in.

## 4.3 A write from the statement to the disk

This section follows `UPDATE accounts SET balance = balance - 10 WHERE id = 42` in a transaction that then commits.

1. **The point path finds the row.** The statement is a prepared single-row update by primary key. The session takes the point path of document 14 section 14.9, which runs without a plan tree. It probes the primary key B-tree and gets the row id.
2. **The transaction layer checks the row.** It reads the version header of the row in its hot store page. If another transaction has an uncommitted change, the statement waits on that transaction's lock, as PostgreSQL does. If the newest committed version is newer than the snapshot in `REPEATABLE READ` or `SERIALIZABLE`, the statement fails with SQLSTATE `40001`.
3. **The row is changed in place.** The old value of `balance` goes into an undo record in the transaction's undo buffer. The new value is written into the page. The page is marked dirty. If the row was in the cold store, the update deletes it there with a delete mark and inserts the new version in the hot store under the same row id. Document 10 section 10.5.
4. **The log record goes to the worker's ring.** The record names the shard, the table, the row id, the column and the new value. It is appended to the log ring of the worker thread that runs the session. Document 11.
5. **The commit gets a timestamp.** At `COMMIT`, the transaction takes a commit timestamp from the hybrid logical clock. It writes a commit record into the same ring. The commit record lists the log positions in other rings that this transaction depends on, which are the rings of transactions whose writes it read or overwrote.
6. **The commit becomes durable.** The worker's ring is flushed with one write and one `fdatasync` or one `io_uring` write with the `RWF_DSYNC` flag, together with the other commits of the same worker in that window. When this ring and every ring in the dependency list are durable to the needed position, the session sends `CommandComplete`. Commits on other workers do not wait for this one.
7. **The page is written later and out of place.** A background task writes the dirty page to a new location in the file and records the new location in the page table. A checkpoint makes the new page table reachable from a header slot. Document 08 section 8.6.

## 4.4 The move from hot to cold

The hot store holds new rows and rows that changed recently. A background task, the mover, watches each table. When a range of row ids has had no change for a set time and holds enough rows for a segment, the mover reads those rows at a snapshot, encodes them into a new segment with summaries, writes the segment, and in one transaction marks the rows as moved in the hot store. Readers that started before the move read the hot pages. Readers that start after it read the segment. The hot pages are freed when no snapshot can see them. Document 10 section 10.6.

The mover decides the sort order of a new segment from the table's chosen sort key (document 10 section 10.7). A bulk load with `COPY` or `INSERT ... SELECT` of many rows writes segments directly and does not pass through the hot store.

## 4.5 Threads and tasks

**The server has one worker for each core.** Each worker is an operating system thread, pinned to its core on Linux. A worker runs a scheduler with four queues in priority order:

1. I/O completions and session tasks that are ready to run a short statement.
2. Morsels of analytic pipelines.
3. Morsels of maintenance work: the mover, index builds, statistics.
4. Idle work: checksum scrubbing and compaction.

A worker takes from queue 2 only when queue 1 is empty, and it checks queue 1 between morsels. A morsel is sized to run in about 1 ms. So a point statement waits at most one morsel behind analytic work on the same worker. This is how one node serves TPC-C and ClickBench at the same time without one hurting the other. Document 14 section 14.11 gives the measured target for the interference.

**A session is a task.** A session has its state, its input and output buffers, and a stack for the execution of its current statement, which is a Rust future. A session is not tied to one worker for its lifetime. It runs where it was woken. An idle session holds only its state and its buffers, at most 64 KiB in total.

**I/O is asynchronous.** On Linux each worker has its own `io_uring` instance. On macOS each worker uses `kqueue` for sockets and a small pool of threads for file I/O. On Windows, which is a later platform, each worker uses I/O completion ports. In WebAssembly there is one thread, and file I/O goes to the Origin Private File System with synchronous access handles.

**The library has no threads unless asked.** When a program opens a file through the library, the default is that the calling thread runs the statement, and a query uses extra threads only from a worker pool that the program enables with a setting. The server and the library use the same scheduler code with a different number of workers.

## 4.6 Memory

The node has one memory budget, set by `rupg.memory_limit`. The default is 25 percent of physical memory for the server and 256 MiB for the library. The budget covers the buffer pool, the undo buffers, the operator memory of all running queries, the plan cache and the catalog cache. A query reserves operator memory from the budget, and it spills to the file when the reservation is refused. The buffer pool shrinks when queries need memory and grows when they release it. This is the unified design of Kuiper, Groß, Boncz and Mühleisen (PVLDB 18, 2025), and it is needed for the memory target of document 02 section 2.6.

`shared_buffers` and `work_mem` exist as settings, because clients set and read them. `shared_buffers` sets the minimum size of the buffer pool. `work_mem` sets the reservation of one operator before it spills. Document 19.

The buffer manager maps the file into a reserved range of virtual memory with one page table entry for each page, as vmcache does (Leis, Alhomssi, Ziegler, Loeck and Dietrich, SIGMOD 2023). Page reads use optimistic version checks and no locks. Eviction uses `madvise(MADV_DONTNEED)`. The exmap kernel module is not required. Where virtual memory reservation is not available, as in WebAssembly, the buffer manager uses a hash table from page number to frame. Document 08 section 8.8.

## 4.7 Identifiers

Every layer uses these identifiers. They are fixed at M1, because the file format stores them.

**OID.** 32 bits, as in PostgreSQL. Built-in objects have the OIDs of the pin. User objects get OIDs from 16384 upward, from a counter in the catalog. In a cluster, the catalog is replicated and the counter is in the catalog, so OIDs are unique across the cluster.

**Shard number.** 16 bits. A single node has shard 0. Document 18 assigns shards in a cluster.

**Row id.** 64 bits: the shard number in the high 16 bits and a local number in the low 48 bits. The local number is assigned by a counter for each table and shard. A row keeps its row id for its life. An update keeps the row id. A row that moves from the hot store to the cold store keeps its row id. The cold store maps a row id range to a segment and a position. Document 10 section 10.2.

**Commit timestamp.** 64 bits, a hybrid logical clock value: 48 bits of physical time in milliseconds since 1 January 2020 and 16 bits of logical counter. 48 bits of milliseconds last about 8,900 years. The logical counter allows 65,536 commits in one millisecond on one node before the clock moves its physical part forward by one millisecond, which the hybrid logical clock rules allow. Snapshots are commit timestamps. A reader sees every version with a commit timestamp at or below its snapshot.

**Transaction id.** 64 bits, local to the node, assigned when a transaction writes first. PostgreSQL clients see a 32-bit `xid` and a 64-bit `xid8`. rupg shows the low 32 bits as `xid` and the full value as `xid8`. `xmin` and `xmax` of a row are the `xid` of the transaction that wrote or deleted the current version. Document 11 section 11.9.

**LSN.** PostgreSQL clients read `pg_lsn` values from functions such as `pg_current_wal_lsn()` and compare them. rupg has no single log, so it shows the commit timestamp of the newest durable commit as the LSN. That value is monotonic and 64 bits wide, which is what the clients need. Document 11 section 11.12.

**Page number.** 64 bits, the offset of the page in the file divided by the page size. The page size is 16 KiB for row pages and index pages. Column data is in extents, which are runs of pages. Document 08.

## 4.8 The rules between layers

**No layer below the analyzer knows SQL text.** The parse tree stops at the analyzer. The optimizer, the executor and the storage see OIDs, types and expressions.

**No layer below the catalog knows PostgreSQL catalog rows.** The virtual `pg_catalog` tables are built from the rupg catalog by the catalog layer. Storage and transactions do not store `pg_class` rows.

**No layer below the sessions knows the transport.** The executor writes results into a sink trait. The wire encoder is one implementation of it, and the library result set is another.

**Every layer has a text form.** Each layer can print its main structure as text and parse it back: the parse tree, the query tree, the physical plan, a segment's metadata, a page, a log record and the compiled pipeline. A test can then build any input for one layer by hand. This is the rule of `../2140/00-README.md` and it is how a result from the literature can be tried in one crate.

**Errors carry a SQLSTATE from the start.** An error type in any layer has a SQLSTATE field. A storage error that reaches a client becomes the SQLSTATE that PostgreSQL would send for the same cause, for example `53100` for a full disk and `XX001` for a page with a bad checksum.

**`unsafe` is local.** Only the crates that document 22 lists may contain `unsafe`: the platform crate, the buffer manager, the kernels, the compile tier, the C API and the WebAssembly bindings. Every `unsafe` block has a `SAFETY` comment, and the crates outside the list have `#![forbid(unsafe_code)]`.

## 4.9 What is lifted from rudb

rupg takes code from rudb where rudb has a measured result. Each lifted file has a header comment with the rudb path and commit. The lifts planned at M1 to M6 are:

| From rudb | Into rupg | Why |
|---|---|---|
| `rudb-kernels` bit-unpack, FOR, delta and filter kernels | `rupg-kernels` | FastLanes kernels with runtime dispatch, measured in rudb |
| `rudb-encoding` and `rudb-compress` ALP, FSST, dictionary and RLE | `rupg-encode` | the encodings of document 09 |
| `rudb-storage` zone maps and global string codes | `rupg-column` | H1 and H2 of document 02 |
| the certified frequency synopses of `storage-v3/11` | `rupg-summary` | H3 of document 02 |
| `rudb-graph` link and CSR structures | `rupg-graph` | the link index of document 13 |
| `rudb-qc-*` compile tiers | `rupg-jit` | the compile tiers of document 14, from M6 |

The lift is a copy. rupg does not take a Cargo dependency on a rudb crate, because rudb's types are DuckDB's types and rudb's crates change for rudb's reasons. Document 24 keeps open whether the kernels later move into a shared crate that both projects use.

## 4.10 Platforms

| Platform | Server | Library | Tier |
|---|---|---|---|
| Linux x86-64 | yes | yes | 1: every gate runs here |
| Linux aarch64 | yes | yes | 1 |
| macOS aarch64 | yes | yes | 2: development and the library |
| Windows x86-64 | yes, from M9 | yes, from M9 | 2 |
| wasm32 with the OPFS | no | yes, from M9 | 2: single thread |

Tier 1 platforms run the full test suite on every change. Tier 2 platforms run it every night. The kernels have an AVX-512 path, an AVX2 path, a NEON path and a portable path, chosen at run time. The WebAssembly build uses the portable path with 128-bit SIMD.

## 4.11 The cluster is beside the stack, not on top of it

A cluster is not a coordinator process in front of single-node servers. Every node runs the same binary with the same layers. The cluster layer adds three things at three points.

1. **Placement.** The catalog stores which shards exist and which nodes hold each shard. The executor asks the placement for the nodes of a table and adds exchange operators to the plan.
2. **Replication.** The log layer sends the log records of a shard to the replicas of that shard through a Raft group. A write is durable when a quorum of the group has it, in place of the local `fdatasync`.
3. **Distributed commit.** The transaction layer commits a transaction that changed one shard with one Raft round. A transaction that changed more than one shard uses a two-phase commit whose prepare records are in the Raft logs of the shards, with the commit decision made without an extra replication round, as in Mako (OSDI 2025).

On a single node the three points are trivial implementations of the same traits. That is what "ready for a cluster" means in this folder: the single-node code calls the traits from M1, and the traits have a cluster implementation from M10. Document 18.
