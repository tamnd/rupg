# 23. Milestones

Written 7 October 2026. Pins: PostgreSQL `REL_19_STABLE` at `7d3d2db7` (19beta4), with RC1 due on 15 October 2026 and 19.0 due on 29 October 2026. The gates come from document 02 section 2.10. The suite counts come from the source tree at the pin. The fuzzing hours and crash counts are the budgets of document 21.

This document puts the work of rupg in thirteen milestones, M0 to M12. For each milestone it gives the scope, the deliverables, the gate, the tests that must pass, what is out of scope, and what the milestone depends on. It then says why the order is what it is, how a milestone closes, how versions are numbered, and what the checklist at each close is.

## 23.1 The rules

**A milestone closes only on a measurement.** A milestone is closed when its report is published in `rupg-bench/reports/` and `rupg-compat/reports/`, and the report shows each number that this document asks for. A feature that is written and not measured does not close a milestone.

**A gate that fails is reported, not moved.** Document 02 section 2.10 gives the rule. The report publishes the number, the gap and the cause. A target changes only after a measurement in document 24 shows that the target is wrong. A failed gate does not stop the next milestone from starting, but the failed milestone stays open until its gate passes or document 24 changes the target.

**Milestones close in order.** Work on a milestone can start before the previous milestone closes. A milestone cannot close before the milestone before it.

**Every number is measured from the milestone that first has it.** The performance table of `rupg-bench` gets a row for each suite when the first milestone can run it. The number is published from then on, also before its gate. This is the rule "performance from the first milestone" of `../2140/compat/postgres/17-the-order-of-work.md`.

**This document has no dates.** A date needs a measured rate of work, and rupg has none yet. The report of each milestone gives the time that the milestone took and the lines that it added against the budgets of document 22. After M2, document 24 can hold a forecast that uses those two numbers.

## 23.2 The milestones at a glance

| Milestone | Name | Gate | Compatibility level reached | Crates that first ship |
|---|---|---|---|---|
| M0 | The harness and the baselines | none | the oracle runs | none in `rupg`. `rupg-compat` and `rupg-bench` |
| M1 | The file | none | none | `rupg-common`, `rupg-types`, `rupg-platform`, `rupg-file`, `rupg-kernels`, `rupg-buffer`, `rupg-tree`, `rupg-log`, `rupg-hot`, `rupg-table`, `rupg-txn`, `rupg`, `rupg-cli` |
| M2 | Connect and introspect | G1 | L1, L2 | `rupg-wire`, `rupg-catalog`, `rupg-pgcatalog`, `rupg-sql`, `rupg-analyze`, `rupg-plan`, `rupg-func`, `rupg-exec`, `rupg-session`, `rupg-server` |
| M3 | SQL and the point path | G4 | L3 tracked | `rupg-opt` |
| M4 | Columns | G2, G3 | | `rupg-encode`, `rupg-summary`, `rupg-column` |
| M5 | Transactions in full | G5 | L4 | |
| M6 | Indexes, graph and compile tiers | none | | `rupg-index`, `rupg-graph`, `rupg-ann`, `rupg-jit`, `rupg-ext`, `rupg-contrib` |
| M7 | Procedures | G6 | L3 full schedule | `rupg-plpgsql` |
| M8 | The 10x | G7, G8, G9, G10 | | `rupg-exchange` |
| M9 | Embedding and shims | none | L1 to L4 on shims 14 to 18 | `rupg-shim`, `rupg-capi`, `rupg-wasm`, `rupg-import` |
| M10 | Replication | none | | `rupg-raft`, `rupg-cluster` |
| M11 | Shards | G11 | | |
| M12 | The long tail | G12 | L1 to L5 on 19 and shims 14 to 18 | |

Gate G1 needs clients to read the catalog with SQL at M2, so the SQL front, a first analyzer and a simple executor ship at M2, and M3 completes them. Section 23.5 says what M2 needs from each.

## 23.3 M0: the harness and the baselines

**Scope.** The three repositories, CI, the oracles and every baseline number that a later gate uses. This is the principle "measure before build" of `../2140/compat/postgres/17-the-order-of-work.md`.

**Deliverables.**

1. `rupg` with the workspace of document 22 section 22.2, `xtask/layers.toml`, `deny.toml`, `clippy.toml` and the per-change jobs of document 21 section 21.16. The crates are empty except for their `lib.rs` headers.
2. The vendored files of document 22 section 22.8 at the pin, with `cargo xtask vendor --check`, and the LALR(1) generator with no conflict on `gram.y`.
3. `rupg-compat` with the oracles for 19 and for 14.24, 15.19, 16.15, 17.11 and 18.6, the proxy, replay, comparison and report. The 48 client traces recorded against the oracle.
4. `rupg-bench` with the harness for ClickBench, TPC-H, TPC-C (HammerDB and the statement form), YCSB and pgbench, and the instruction count runner.
5. Baselines measured by us on the machines of document 20: PostgreSQL, ClickHouse and DuckDB on ClickBench on `c6a.4xlarge` and `c6a.metal`, DuckDB on TPC-H SF100, PostgreSQL on TPC-C and YCSB. Document 03 gives the published numbers that these must agree with.

**Gate.** No gate of document 02. The exit check is that the oracle replays every recorded trace against itself with no difference after the unstable filter, and that each baseline number in document 03 has a remeasured number in `rupg-bench/reports/`.

**Out of scope.** Engine code.

**Depends on.** Nothing. The 19.0 release on 29 October 2026 falls inside M0, and the pin moves from the beta to RC1 and then to 19.0 with a full rerun each time.

## 23.4 M1: the file

**Scope.** The single `.rupg` file and everything that makes a write durable: the format, the header slots, the buffer manager, the per-worker log, recovery, the hot store, the row id with its shard field, the HLC commit timestamp, a first MVCC with snapshot reads, and a Rust key-value API over typed tables.

**Deliverables.**

1. The file format of document 08: header slots, pages of 16 KiB, extents, free space, checksums, out-of-place writes and checkpoints (document 08 section 8.6).
2. The buffer manager of document 08 section 8.8 with the memory limit of document 04 section 4.6.
3. The per-worker log rings, group commit and recovery of document 11.
4. The hot store of document 10, with row ids of 16 bits of shard and 48 bits of local id, and the HLC of 48 bits of milliseconds and 16 logical bits.
5. The `rupg` facade with a Rust API that opens a file, creates a table with typed columns, and does insert, update, delete, get and range scan in transactions.
6. `rupg check` in the `rupg` binary.
7. The platform traits of document 21 section 21.10 with the operating system and the simulation implementations.

**Gate.** No gate of document 02. The milestone reports the commit latency at one writer, the commits per second at one writer per core on `gpc`, the size of an empty file, and the recovery time after 1 GiB of log. These numbers set the floor for G4 and G5.

**Tests.** 1 million crash files with no lost commit (document 21 section 21.9.4). 24 CPU hours each on `file_open`, `page_decode` and `log_replay` with no open find. loom tests for the buffer latches and the log ring reservation. dm-log-writes and LazyFS runs on ext4 and XFS.

**Out of scope.** SQL, the network, the cold store.

**Depends on.** M0.

## 23.5 M2: connect and introspect

**Scope.** A real client can connect, authenticate, read the catalog and see the schema. Protocols 3.0 and 3.2, TLS, SCRAM-SHA-256, the virtual catalog, and the text and binary forms of the types.

**Deliverables.**

1. `rupg-wire` and `rupg-server`: startup, `NegotiateProtocolVersion`, SSL negotiation, SCRAM, the simple and extended query flows, cancel, and `ParameterStatus`.
2. `rupg-session` with the 438 settings, `SET`, `SHOW` and `RESET`.
3. `rupg-catalog` and `rupg-pgcatalog`: the built-in objects with their OIDs, the 64 catalogs, the 86 system views and the 65 `information_schema` views, as virtual tables.
4. The types of document 07 with text and binary forms equal to the oracle.
5. The SQL front for what catalog queries and the gate schema need. The parser is the full vendored grammar from the start, because the grammar is one file. The analyzer, the plan and the executor at M2 handle `SELECT` with joins, subqueries, `CASE`, casts to `regclass` and `regtype`, and the functions that the 48 recorded traces call on the catalog, for example `format_type`, `pg_get_expr`, `pg_get_indexdef` and `pg_get_constraintdef`. DDL creates tables, columns, constraints and index definitions.

**Gate.** G1: 7 clients connect and show the schema. Document 21 section 21.4.2 names the seven: psql, pgAdmin 4, DBeaver, psycopg 3, node-postgres, pgx and asyncpg.

**Tests.** The L1 traces of the 48 clients, with the share of identical messages published for each. The L2 comparison of every catalog row for the gate schema. The 9 `libpq_pipeline` tests. The `pg_dump` round trip of document 21 section 21.4.1. 24 CPU hours each on `wire_frontend`, `wire_startup`, `type_input` and `type_recv`. The idle session test of document 21 section 21.13 against the 64 KiB limit of document 06 section 6.4.

**Out of scope.** DML, user data in quantity, the optimizer.

**Depends on.** M1.

## 23.6 M3: SQL and the point path

**Scope.** The SQL that applications run on rows: the full analyzer, `INSERT`, `UPDATE`, `DELETE`, `MERGE` and `RETURNING`, MVCC at `READ COMMITTED` and `REPEATABLE READ`, prepared statements and the plan cache, the point path, the first optimizer, and the vectorized executor version 1 on the hot store.

**Deliverables.**

1. `rupg-analyze` for the full grammar, with the error for each statement that rupg does not yet run.
2. MVCC with in-place update and undo (document 11), the xid exposure of document 11 section 11.9 and the LSN of document 11 section 11.12.
3. The point path of document 14, which runs a prepared single-row statement with no heap allocation.
4. The vectorized executor version 1: scans, filters, projections, hash join, hash aggregate, sort, limit, window functions.
5. `rupg-opt` version 1 with statistics from `ANALYZE` and `EXPLAIN` output in the format of document 15 section 15.9.
6. The built-in functions that the regression suite calls most, in the order of the count of calls.

**Gate.** G4: YCSB workloads A, B, C and F at 3x less server CPU per operation than PostgreSQL, without pipelining, with the rules of document 20.

**Tests.** The regression suite, with the pass count of the 239 tests published and ratcheted. sqllogictest. The differential generator with TLP, NoREC and PQS each night. 48 CPU hours on `sql_parse`. 1 million more crash files with DML and DDL.

**Out of scope.** The cold store, `SERIALIZABLE`, PL/pgSQL.

**Depends on.** M2.

## 23.7 M4: columns

**Scope.** The cold store and the path that fills it: the column segments, the encodings, the mover, `COPY` for bulk loads, H1 (scalars from segment summaries), H2 (strings as global codes) and zone maps.

**Deliverables.**

1. `rupg-encode` with the encodings and the chooser of document 09, lifted from rudb (document 22 section 22.7).
2. `rupg-column` with segments, the global dictionaries of document 09 section 9.5 and the segment summaries of document 09 section 9.7.
3. The mover of document 04 section 4.4, in `rupg-txn`.
4. `COPY FROM` and `COPY TO` in text, CSV and binary, with a load path that writes segments directly.
5. The vectorized executor on encoded data, with the summary reads of document 12 section 12.6.
6. Every function that the 43 ClickBench queries call.

**Gate.** G2: the ClickBench hot sum on `c6a.4xlarge` below ClickHouse's number, 17.53 s, remeasured at M0. G3: the file below 9.45 GB after the ClickBench load.

**Tests.** The layout equivalence oracle of document 21 section 21.4.5 with all five layouts. The first run of document 21 section 21.8: delete every summary and rerun every suite, with answers that match. 48 CPU hours on `segment_decode`. 1 million more crash files with the mover and `COPY`. The ClickBench answers equal to the oracle's answers.

**Out of scope.** H3 to H6, the compile tiers.

**Depends on.** M3.

## 23.8 M5: transactions in full

**Scope.** The rest of the transaction model: `SERIALIZABLE` with SSI, savepoints, row locks (`FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR SHARE`, `FOR KEY SHARE`, `NOWAIT`, `SKIP LOCKED`), advisory locks, `LISTEN` and `NOTIFY`, and two-phase commit with `PREPARE TRANSACTION`.

**Deliverables.** The transaction layer of document 11 at full scope. The deadlock detector, with the `40P01` error. The lock views `pg_locks` and `pg_stat_activity` with the columns that the isolation tester reads.

**Gate.** G5: TPC-C with statements (no stored procedures) at 3x the NOPM per core of PostgreSQL. PostgreSQL 18.1 gave 431,648 NOPM on a 48-core EPYC 9454P in the Small Datum report of 15 February 2026, which is about 9,000 NOPM per core. The gate uses our own measurement from M0, not that number.

**Tests.** The 135 isolation specs with the pass count published. Hermitage. Elle on a single server at each level (document 21 section 21.12). The simulation at 100 CPU hours each night. 1 million more crash files with two-phase commit, savepoints and `NOTIFY`. The TPC-C consistency conditions after each run.

**Out of scope.** Indexes other than the B-tree that M3 needs for primary keys, PL/pgSQL.

**Depends on.** M4. M5 could start after M3, but the mover of M4 is a transaction and its interaction with SSI must be tested before the isolation suite counts.

## 23.9 M6: indexes, graph and compile tiers

**Scope.** Every index access method of PostgreSQL on rupg storage, the two rupg index kinds, and the compile tiers.

**Deliverables.**

1. `rupg-index`: B-tree with every option that PostgreSQL 19 has, hash, GIN, GiST, SP-GiST and BRIN, with the operator classes of the built-in types.
2. `rupg-graph`: the `USING link` index and the CSR adjacency of document 13, lifted from rudb. The property graph syntax is off by default (document 13).
3. `rupg-ann`: the `USING vector` index, with `hnsw` and `ivfflat` as names that map to it.
4. `rupg-jit`: the interpreter tier, the direct x86-64 tier and the Cranelift tier of document 14, lifted from rudb's query compiler.
5. `rupg-ext` with `CREATE EXTENSION` over built-in extensions, and the first ports in `rupg-contrib`: `btree_gist`, `btree_gin`, `pg_trgm` and the pgvector surface (document 16).

**Gate.** No gate of document 02. The report publishes the index pass count of the regression suite, the recall and the queries per second of the vector index against pgvector at the same parameters, and the gain of the compile tiers on ClickBench and TPC-H SF10 against the interpreted executor.

**Tests.** The index-related regression tests. The tier equivalence tests of document 21 section 21.5. 48 CPU hours on `jit_verify`. 1 million more crash files with each access method. The second run of document 21 section 21.8, with every index that is not a constraint dropped.

**Out of scope.** PL/pgSQL, the 10x work.

**Depends on.** M5, because each index must follow the visibility and locking rules of the full transaction model.

## 23.10 M7: procedures

**Scope.** PL/pgSQL, triggers, event triggers, rules, the extension API and the first contrib extensions. The full regression schedule.

**Deliverables.** `rupg-plpgsql` from the vendored `pl_gram.y`. Triggers of every kind (row and statement, before, after and instead of, transition tables, constraint triggers). The rule system for views and for `CREATE RULE`. The rest of the contrib extensions of document 16. `rupg-ext` and the first extensions (`btree_gist`, `pg_trgm` and the pgvector surface) arrive at M6, because documents 12 and 13 need them.

**Gate.** G6: TPC-C with stored procedures at 10x the NOPM per core of PostgreSQL. The regression suite runs the full parallel schedule.

**Tests.** The regression suite with the pass count published. The contrib regression schedules of the extensions that ship. 48 CPU hours on `plpgsql_parse`.

**Out of scope.** The summaries of H3 to H6.

**Depends on.** M6. Many regression tests and TPC-C procedures use indexes.

## 23.11 M8: the 10x

**Scope.** H3 to H6 of document 02, the exchange operators for parallel plans, and the tuning work that the gates need.

**Deliverables.**

1. H3: the certified frequency synopses (document 12 section 12.8).
2. H4: the gram summaries and the code-level `LIKE` (document 12 section 12.9).
3. H5: the sort order and the sorted projections chosen at load (document 10 section 10.7).
4. H6: the per-distinct function results (document 14 section 14.6).
5. `rupg-exchange` and the parallel plans of document 14.

**Gate.** G7: the ClickBench hot sum at most 1.753 s and the cold sum at most 11.37 s on `c6a.4xlarge`. G8: 10x on peak memory and CPU time against each baseline, and the file below 7.67 GB. G9: TPC-H SF100 at 10x DuckDB's hot total. G10: the ClickBench hot sum at most 0.440 s on `c6a.metal`.

**Tests.** Document 21 section 21.8 on every suite with every new summary. The layout equivalence oracle with H5 projections. The memory limit tests of document 21 section 21.13. The answers of ClickBench and TPC-H equal to the oracle.

**Out of scope.** New SQL surface.

**Depends on.** M7. The gates are measured on an engine whose surface no longer changes much, so the numbers stay true as the long tail is added. Document 02 says that G9 is the gate most likely to miss.

## 23.12 M9: embedding and shims

**Scope.** Every way into rupg except the cluster: the C API with the libpq exports, WebAssembly, Windows, the shims for 14 to 18, the importer for PostgreSQL data directories and `pg_dump` archives, and logical replication in and out.

**Deliverables.**

1. `rupg-capi` with the C API of document 17 and the libpq exports, so a program linked to libpq opens a file in process.
2. `rupg-wasm` with the OPFS storage, single thread.
3. The Windows build at tier 2 (document 04 section 4.10).
4. `rupg-shim` with `rupg.compat_version` for 14 to 18.
5. `rupg-import` for a stopped PostgreSQL data directory of versions 14 to 19 and for `pg_dump` archives in each format.
6. Logical replication: rupg subscribes to a PostgreSQL publication, and PostgreSQL subscribes to a rupg publication.

**Gate.** No gate of document 02. The report publishes the pass sets of the suites of each shim version (document 05 section 5.6) against the oracle of that version, and the in-process latency of a point read through the libpq exports.

**Tests.** The suites for each shim version. The clients that link libpq, run in process through the exports. 48 CPU hours each on `capi_calls` and `import_dump`. The tier 2 nightly runs on Windows and wasm32.

**Out of scope.** Raft, shards.

**Depends on.** M8. The C API freezes a public interface, so it comes after the engine's internals have settled.

## 23.13 M10: replication

**Scope.** A Raft group per shard, read replicas, failover, point-in-time recovery and backup.

**Deliverables.** `rupg-raft` with elections, log replication, snapshots and membership changes. The cluster implementations of the log traits of document 04 section 4.11, so a write is durable on a quorum. Read replicas with bounded staleness. Failover with no lost acknowledged commit. Point-in-time recovery to an HLC timestamp. Online backup to a file and to object storage.

**Gate.** No gate of document 02. The report publishes the commit latency and the TPC-C NOPM on a three-node group against M5, and the failover time.

**Tests.** Jepsen on 3 to 5 nodes with partitions, kills, pauses and clock skew (document 21 section 21.12). The simulation at 500 CPU hours each night. 1 million more crash files with Raft log and snapshot writes.

**Out of scope.** More than one shard.

**Depends on.** M9 for the order of closing. The Raft work depends technically only on M5, and it can start early.

## 23.14 M11: shards

**Scope.** Many shards across many nodes: placement, distributed commit, distributed query and rebalancing.

**Deliverables.** `rupg-cluster` with the shard map in the catalog, hash and range placement, the distributed commit of document 04 section 4.11, distributed plans with exchange operators between nodes, and online rebalancing that moves a shard with no write outage longer than the budget of document 18.

**Gate.** G11: TPC-C on 2 to 16 nodes at least 0.8 N times the NOPM of one node.

**Tests.** Jepsen on up to 16 nodes with the list-append, register, bank and TPC-C workloads, and with membership changes and rebalancing during the run. Elle checks on every simulation run.

**Out of scope.** Geo-distribution.

**Depends on.** M10.

## 23.15 M12: the long tail

**Scope.** Everything that the 100 percent claim needs and that no earlier milestone did: the rest of the 3,414 functions, the rest of the contrib extensions that document 16 lists, the TAP subset, the remaining client suites, and every `expected-fail.txt` line.

**Gate.** G12: 100 percent on L1 to L5, on PostgreSQL 19 and on the shims for 14 to 18.

**Tests.** Every suite of document 21. The 1.0 conditions of document 21 section 21.17, including 30 days of fuzzing on every target with no new find.

**Out of scope.** Nothing in the documents of this folder. A feature that a later PostgreSQL release adds is the next milestone after 1.0.

**Depends on.** M11.

## 23.16 Why the order is what it is

**The harness comes first.** No number in this folder means anything without the baselines and the oracle. rudb's compat work found that measuring first changed the order of the work (`../2140/compat/postgres/17-the-order-of-work.md`). M0 makes every later gate a comparison with a number that we measured.

**The file comes before SQL.** Every layer stands on the file, and the file format is the hardest thing to change after users have files. The HLC and the shard field are in M1 because a row id written without a shard field can never get one. This is the meaning of "ready for a cluster" in document 04 section 4.11.

**Connect before query.** A client can test nothing until it connects. Many clients read the catalog before they send any user query, so the catalog comes with the connection. This is "follow the client" and "catalog before dialect" of `../2140/compat/postgres/17-the-order-of-work.md`.

**The point path at M3, not at M8.** Gate G4 measures the CPU per operation. A slow protocol layer is hard to fix late, because its cost is spread over every message. The point path sets the cost floor while the code is still small.

**Columns at M4, before full transactions.** The cold store is the largest bet in the design. G2 at M4 asks only for a hot sum below ClickHouse, not 10x. If the format cannot reach G2, the design must change before indexes and procedures are built on it.

**Transactions before indexes.** Each index must follow the visibility and lock rules. Building six access methods on a partial transaction model would mean building them twice.

**Procedures before the 10x.** G6 needs stored procedures, and the regression suite needs PL/pgSQL. The 10x work comes after the surface is mostly complete, so the gates measure the engine that ships.

**Embedding after the 10x.** The C API and the libpq exports are a public interface. Changing one after users link to it is expensive. They wait until the internals that they expose have settled.

**The cluster last.** The single-node code calls the cluster traits from M1, so the cluster adds implementations and does not change the stack. The cluster can then come late without a rewrite.

## 23.17 Versions

The version of a release is `0.M.patch`. The release at the close of milestone M is `0.M.0`, and fixes on that line are `0.M.1`, `0.M.2` and on. M0 publishes no crate. 1.0 follows M12 when the conditions of document 21 section 21.17 hold. A version number says what was measured, so a release that is not at a closed milestone has no `0.M.0` number.

The file format has its own version in the header slots (document 08). From M1 on, every release can open every file that an earlier release of the same `0.M` line wrote. Between `0.M` lines the file format may change, and `rupg upgrade` writes a new file in the new format from the old one. From 1.0 on, the format changes only with an upgrade path that the crash tests cover.

## 23.18 The checklist at each close

The report of each milestone answers each line of this list with a number or a link. A line that does not apply is marked so, with the reason.

| Item | From |
|---|---|
| Each gate of the milestone has a number with the five parts of document 02 section 2.2 | M2 |
| Each pass count is at or above its ratchet (document 21 section 21.15) | M2 |
| Each fuzz target of the milestone has its CPU hours with no open find (document 21 section 21.7) | M1 |
| The crash count of document 21 section 21.9.4 is reached with no lost commit | M1 |
| No open bug of the classes wrong answer, lost commit or corrupt file | M1 |
| Every suite gives the same answers with every summary deleted (document 21 section 21.8) | M4 |
| The layout equivalence oracle ran each night of the milestone with no open find | M4 |
| The instruction count set of document 21 section 21.14 has the new queries | M1 |
| Each baseline that a gate used was remeasured within 30 days | M2 |
| The lines of each crate are in the report against its budget (document 22 section 22.14) | M1 |
| `cargo xtask lifts` reports no rudb change that has not been ported or declined | M1 |
| Each document whose content changed is updated, and each miss has an entry in document 24 | M0 |
| The release `0.M.0` is tagged, and the reproducible build matched | M1 |

The 30 days for baselines is a budget. Baselines change when the baseline systems release, and a gate against a baseline that is a year old does not show what the requirement asks.
