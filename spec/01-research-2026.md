# 1. Research as of October 2026

Written 7 October 2026. The PostgreSQL facts are pinned to `REL_19_STABLE` at `7d3d2db7` (19beta4) and to the PostgreSQL release notes and news pages read on 7 October 2026. The ClickBench numbers come from the result files of `github.com/ClickHouse/ClickBench` at `e6bda4e` (6 October 2026). Each paper is cited with its authors, venue and year. A fact that we could not confirm in a primary source is marked UNVERIFIED. A claim that comes only from a vendor and has no peer review is marked VENDOR.

This document lists the systems and the papers that bear on rupg, and for each one it says what the result forces in the design. A "Forces" line names the document that carries the decision. The list is not a survey. An item is here only if it changes a decision, confirms a decision that would otherwise be a guess, or rules out an option. Section 1.10 lists the options that we rejected and the evidence for each rejection.

## 1.1 How to read this document

Each item has three parts. The first part is the citation. The second part is the measured result that matters, with the number. The third part is the "Forces" line. When a result is from a vendor blog, its number is a claim about the vendor's system. We do not use it as a baseline, because document 00 says a baseline is a number that we measured.

PostgreSQL itself comes first because it is the target. Systems that are compatible with PostgreSQL come second because they show which parts of the target other teams found hard. The papers follow, grouped by the part of the design that they affect: storage formats, storage engines, distributed transactions, execution, graph and vector indexes, and embedded files.

## 1.2 PostgreSQL 18 and 19

### 1.2.1 Release status

**PostgreSQL 19 is at beta 4.** Beta 4 was released on 24 September 2026 (postgresql.org news item 3386). On 28 September 2026 Jonathan Katz wrote on pgsql-hackers that RC1 is planned for 15 October 2026 and GA for 29 October 2026. The pin of this folder is the beta 4 commit `7d3d2db7`. When 19.0 is tagged, the pin moves to the tag in one change.

**PostgreSQL 18 is the current stable major version.** 18.0 was released on 25 September 2025. The current minor version is 18.6, released on 13 August 2026 (postgresql.org/support/versioning). The supported set today is 14 to 18. PostgreSQL 14 reaches end of life on 12 November 2026. PostgreSQL 13 reached end of life on 13 November 2025 with 13.23. After 19.0 ships, the supported set is 15 to 19.

Forces: the version shim of document 05 covers 14 to 18 at M9. Version 14 is in the shim because many managed services still run it after its end of life, and its catalog is the oldest that the clients of `../2140/compat/postgres/13-clients-and-tools.md` still test. Version 13 is not in the shim.

### 1.2.2 What PostgreSQL 18 added that a reimplementation sees

The PostgreSQL 18 release announcement of 25 September 2025 lists these changes. Each one is visible to a client or to a test, so rupg must implement it.

**Asynchronous I/O.** The setting `io_method` takes `worker`, `io_uring` or `sync`. The announcement claims up to 3x faster reads from storage. There is a new view `pg_aios`. rupg has its own I/O layer, but the setting and the view must exist, because clients and monitoring tools read them.

**B-tree skip scan.** A multicolumn B-tree index can be used when the query has no condition on the leading column. This changes plans that the regression suite prints with `EXPLAIN`.

**Virtual generated columns.** Generated columns are virtual by default in 18. `STORED` is optional.

**`uuidv7()`.** It is new, and `uuidv4()` is an alias of `gen_random_uuid()`.

**Temporal constraints.** `WITHOUT OVERLAPS` on a primary key or a unique constraint, and `PERIOD` on a foreign key. These need an exclusion check on ranges, which needs a GiST-like index in rupg (document 12).

**`OLD` and `NEW` in `RETURNING`.** They are allowed for `INSERT`, `UPDATE`, `DELETE` and `MERGE`.

**OAuth authentication.** The method `oauth` uses validator modules. MD5 password authentication is deprecated.

**Protocol 3.2.** This is the first new protocol version since PostgreSQL 7.4 in 2003. The only change is a cancel key of variable length in `BackendKeyData` and `CancelRequest`.

**Other changes.** `initdb` turns on data checksums by default, `pg_upgrade` keeps planner statistics, and there is a `PG_UNICODE_FAST` collation, a `casefold()` function and `CREATE FOREIGN TABLE ... LIKE`.

Forces: documents 06 and 07 implement each item. The temporal constraints force an exclusion constraint implementation at M6, not later.

### 1.2.3 Protocol facts for a compatible server

The protocol overview of the PostgreSQL 19 documentation (postgresql.org/docs/19/protocol-overview.html) gives four facts that a server must follow.

1. The newest protocol version is 3.2. libpq still requests 3.0 by default.
2. Version 3.1 is reserved and never used, because old versions of PgBouncer claimed to support it.
3. Version 3.9999 is reserved for "greasing". A server must not treat it as a special case. It must answer with `NegotiateProtocolVersion` and offer the highest version that it supports.
4. During the 19 beta period, libpq sent grease parameters by default. The PostgreSQL 19 release notes say that this stops at 19 GA.

CockroachDB v26.1, released on 18 February 2026, had to add 3.2 negotiation, because clients built against PostgreSQL 18 libpq failed with "unknown protocol version" (cockroachlabs.com/docs/releases/v26.1). pgJDBC 42.7.6 and later accept `protocolVersion=3.2`.

Forces: document 06 implements negotiation for 3.0 and 3.2 at M2, with a test that sends 3.9999 and unknown `_pq_.` parameters. The CockroachDB issue shows that a server which skips negotiation breaks on the next libpq release, not on today's.

### 1.2.4 What PostgreSQL 19 removed before GA

The release team removed these features before beta 4: SQL/PGQ property graph queries, online changes of the data checksum setting, `FOR PORTION OF` for temporal `UPDATE` and `DELETE`, `MERGE PARTITIONS` and `SPLIT PARTITIONS`, and the functions `pg_get_role_ddl()`, `pg_get_tablespace_ddl()` and `pg_get_database_ddl()`. Secondary sources report that `GROUP BY ALL` was removed in beta 3. `gram.y` at the pin (20,077 lines) has no `GROUP BY ALL` production. `ALL` after `GROUP BY` parses only as the standard set quantifier before a list of keys. That removal is UNVERIFIED against a primary source.

Forces: SQL/PGQ is not in the target. Document 13 makes property graph syntax a rupg addition that is off by default, and document 05 counts it outside the compatibility denominators. `FOR PORTION OF` is not in the target either. If it returns in PostgreSQL 20, it becomes work for the pin move to 20.

### 1.2.5 What PostgreSQL 19 keeps that a client sees

The PostgreSQL 19 release notes (postgresql.org/docs/19/release-19.html, "as of 2026-09-14") list the changes below. A server that claims to be 19 must have them.

**New commands and clauses.** `REPACK` and `REPACK CONCURRENTLY` replace `VACUUM FULL` and `CLUSTER`. `WAIT FOR LSN` is new. `INSERT ... ON CONFLICT DO SELECT ... RETURNING` is new. `IGNORE NULLS` and `RESPECT NULLS` apply to `lead`, `lag`, `first_value`, `last_value` and `nth_value`.

**COPY.** `COPY TO ... (FORMAT json)` with `FORCE_ARRAY` is new. So are `COPY FROM ... ON_ERROR SET_NULL`, a header that spans several lines, and `COPY TO` on a partitioned table.

**Types and functions.** A new type `oid8`, casts between `bytea` and `uuid`, the type `regdatabase`, the functions `tid_block()` and `tid_offset()`, `random(min, max)` for `date`, `timestamp` and `timestamptz`, the encodings `base64url` and `base32hex`, `range_minus_multi()`, `error_on_null()` and more jsonpath string methods.

**Changed defaults and behaviors.** `standard_conforming_strings` is always on and `escape_string_warning` is gone. RADIUS authentication and the `MULE_INTERNAL` encoding are gone. JIT is off by default. `max_locks_per_transaction` goes from 64 to 128. `json_array()` over zero rows returns `[]` and not NULL. `default_toast_compression` is `lz4`. An MD5 login issues a warning. The wait event type `BUFFERPIN` is renamed `BUFFER`.

**Catalog changes.** New views `pg_stat_lock`, `pg_stat_recovery`, `pg_stat_autovacuum_scores` and `pg_dsm_registry_allocations`. A `stats_reset` column in many statistics views. `pg_stat_subscription_stats.sync_error_count` is renamed to `sync_table_error_count`. `pg_stats` gains the table OID and the attribute number. `pg_available_extensions` gains `location`.

**Server features.** Worker autoscaling for asynchronous I/O (`io_min_workers`, `io_max_workers`), parallel autovacuum, `effective_wal_level`, server-side SNI through `pg_hosts.conf`, and OAuth validator improvements.

Forces: the changed defaults are the main input to the version shim of document 05. `standard_conforming_strings`, `json_array()` over zero rows and the renamed column are each a behavior that differs between 18 and 19, so each one is a shim entry. `REPACK` maps to the compaction of document 10. `WAIT FOR LSN` waits for the LSN of document 11 section 11.12. JIT off by default matters for `EXPLAIN` text, and document 15 owns it.

## 1.3 Systems that are compatible with PostgreSQL

### 1.3.1 Engines written from scratch

**pgrust.** A clean-slate Rust rewrite of PostgreSQL by Michael Malis and Jason Seibel, licensed AGPL-3.0 (pgrust.com, github.com/malisper/pgrust). It uses threads instead of processes and a vectorized, push-based executor with a JIT. It speaks the PostgreSQL protocol and v0.1 can start on an existing PostgreSQL data directory. The authors report that v0.1, around 9 July 2026, passed all 46,066 regression queries of PostgreSQL 18.3, and that v0.3 targets 18.6 and cut the divergences on a corpus of 164,000 cases from 1,454 to 11. They describe it as not ready for production. Its ClickBench hot sum on `c6a.4xlarge` is 34.88 s from a run of 1 August 2026, with 17.56 GB on disk (`../2140/03-baselines.md` section 3.2). The regression and divergence claims are the authors' own and are UNVERIFIED.

Forces: pgrust shows that a Rust engine can reach the regression suite of a PostgreSQL major version in months, so document 23 does not treat L3 as the long pole. It also shows that a row engine with a vectorized executor is 2x slower than ClickHouse on ClickBench, so a faithful row engine is not enough for axis 2. Its license is AGPL, so the clean room rule of document 00 applies: nobody who works on rupg reads its source.

**CedarDB.** The commercial version of Umbra, written from scratch with no PostgreSQL code. It speaks protocol 3.0, implements many system tables and does not load PostgreSQL extensions. Secondary notes from early 2026 report a `server_version_num` of 160001, which is PostgreSQL 16 (UNVERIFIED). The release of 29 September 2026 added `inet` and savepoints (cedardb.com/docs/releases). On ClickBench `c6a.4xlarge` its hot sum is 28.99 s, its cold sum is 200.71 s and it stores `hits` in 8,455,565,312 bytes (run of 15 August 2026).

Forces: CedarDB is the evidence that the fastest analytic engine design, Umbra at 7.41 s, loses about 4x of its ClickBench speed when it is packaged as a PostgreSQL server with PostgreSQL semantics and its full transactional machinery. Document 14 must keep the analytic path free of the costs of the transactional path, and document 20 measures rupg through the PostgreSQL protocol, not in process, so that the same loss is visible if it happens.

**CockroachDB.** A Go reimplementation with protocol 3.0 and, since v26.1, 3.2 negotiation. It is under the CockroachDB Software License since 24.3. v26.3 is the current stable version. On ClickBench `c6a.4xlarge` its hot sum is 10,965.71 s (run of 18 May 2025).

Forces: CockroachDB's parallel commits (SIGMOD 2020) are background for document 18. Its ClickBench number shows that a sharded row store with a key-value layer underneath is slower than PostgreSQL itself on analytics. Document 18 must not put a key-value layer under the cold store.

### 1.3.2 PostgreSQL with a different storage or executor

**pg_duckdb.** An extension that embeds DuckDB as the executor, with one DuckDB instance per backend. v1.0.0 was released on 4 September 2025 on DuckDB 1.3.2, and v1.1.1 is the latest (github.com/duckdb/pg_duckdb/releases). On ClickBench `c6a.4xlarge` it runs at 52.49 s hot over Parquet. Over the PostgreSQL heap it runs at 11,524.80 s hot, which is the same as PostgreSQL.

Forces: the heap number is the clearest evidence in this list that the executor cannot fix the storage. A vectorized engine over row pages runs at the speed of the row pages. Document 10 puts the cold store in the same table, not in a side file.

**pg_mooncake.** A columnstore extension with Iceberg and Delta tables and real-time mirroring through Moonlink. Databricks acquired Mooncake Labs on 1 October 2025. On ClickBench `c6a.4xlarge` version 17-v0.1.0 runs at 61.01 s hot. On `c6a.metal`, 13 of 43 queries failed.

**OrioleDB.** A table access method with an undo log, no `VACUUM`, 64-bit transaction ids and index-organized tables. It still needs a patched PostgreSQL. Beta 18 is based on 16.15, 17.11 and 18.6, and Supabase opened a public beta on 1 October 2026 that is "not for production" (supabase.com/changelog). Its authors claim 5.5x over the heap on TPC-C at 500 warehouses and 5x on upserts. A later Supabase summary says "up to 1.8x". Both claims are VENDOR. On ClickBench `c6a.4xlarge` it runs at 18,345.67 s hot, slower than the heap.

Forces: OrioleDB is the evidence that undo-based MVCC with in-place updates removes a large part of PostgreSQL's OLTP cost while keeping PostgreSQL semantics. Document 11 uses the same model. Its ClickBench number shows again that the row layout, not the MVCC model, sets the analytic speed.

**TigerData, formerly Timescale.** Hypercore stores recent data as rows and older data as column chunks. The column store is on by default since 2.23. On ClickBench `c6a.4xlarge` it runs at 569.40 s hot with the column store and 8,826.67 s without it.

Forces: a row and column hybrid inside PostgreSQL gives about 20x over the heap and stays about 30x behind ClickHouse. The column chunks are compressed but the executor is PostgreSQL's. Document 14 runs on encoded vectors, not on rows decoded from chunks.

**AlloyDB.** PostgreSQL with a separate storage layer and an in-memory columnar engine that it fills automatically next to the heap. PostgreSQL 18 became GA on AlloyDB on 25 March 2026 (AlloyDB release notes). Google's benchmark guide (updated 5 October 2026) gives 716,138 NOPM on TPC-C at 64 vCPU fully cached and 467,583 tps on select-only pgbench at 64 vCPU.

Forces: AlloyDB keeps a column copy in memory beside the row copy, which doubles the memory for hot data. rupg keeps one copy in either the hot store or the cold store (document 10), because axis 3 counts memory.

### 1.3.3 Embedded PostgreSQL

**PGlite.** PostgreSQL compiled to WebAssembly as a TypeScript library for browsers, Node, Bun and Deno. It runs one user with one connection and is about 3 MB gzipped. It stores data in IndexedDB or the file system. Electric, which makes PGlite, joined Databricks on 11 August 2026. Weekly downloads grew from 1 million to 13 million in 12 months (databricks.com blog). The PostgreSQL version that it is built on is UNVERIFIED.

Forces: PGlite is the evidence that demand for an embedded PostgreSQL is large and real. It is the PostgreSQL row engine with one connection. Document 17 gives rupg the same WebAssembly target with the columnar engine, several connections in one process, and one file.

### 1.3.4 Sharded and distributed PostgreSQL

**Aurora DSQL.** Brooker, Bowes, Hershey, van der Merwe, Morle, Strydom and others, arXiv 2607.13276, July 2026. DSQL is serverless and active-active. Compute, commit and storage are separate services. It uses optimistic concurrency with strong snapshot isolation only. Precise synchronized time lets reads skip coordination, and adjudicators with a journal decide commits. The paper gives a p99 of about 2 ms for a primary-key `SELECT` and about 3 ms for an `UPDATE`. A `COMMIT` across regions (us-east-1 and us-east-2 with a witness in us-west-2) takes about 30 ms, 7.4 ms in one region, and less than 1.5 ms at p99 for a read-only transaction. DSQL became GA on 27 May 2025 and is compatible with PostgreSQL 16. A third party reported limits of 10,000 rows per transaction and no foreign keys in November 2025. Those limits are UNVERIFIED for today.

Forces: DSQL is the closest prior art to the cluster of document 18. It shows that a PostgreSQL-compatible sharded system can coordinate only at commit and still serve point statements in milliseconds. rupg must offer snapshot isolation in the cluster with the same structure. It must also offer `SERIALIZABLE`, which DSQL does not, and document 18 must justify its cost.

**YugabyteDB.** DocDB, a Raft and LSM storage layer, with a forked PostgreSQL query layer. It rebased from PostgreSQL 11.2 to 15 in v2.25 (30 January 2025). v2026.1.0.0 shipped on 29 June 2026. On ClickBench `c6a.4xlarge` it runs at 51,701.64 s hot.

Forces: like CockroachDB, a key-value storage layer under a row model is the slowest design on analytics in the whole board. Document 18 shards the hybrid table of document 10, not a key-value store.

**Citus.** A coordinator and worker extension. Citus 14.0 added PostgreSQL 18 on 17 February 2026, and 14.x supports 16 to 18. On ClickBench `c6a.4xlarge` it runs at 1,745.08 s hot.

**Supabase Multigres.** "Vitess for Postgres", Apache 2. A gateway, a pooler and a Kubernetes operator in front of unmodified PostgreSQL. v0.1 alpha shipped on 4 June 2026 with no sharding yet.

**PlanetScale Neki.** A sharding router that speaks the PostgreSQL protocol in front of real PostgreSQL shards, each with one primary and at least two replicas over three zones. Announced on 11 August 2025, platform preview on 10 September 2026. The claim of 118.5 million queries per second over 512 shards is VENDOR. That Neki is closed source is UNVERIFIED.

**Neon and Lakebase.** Compute is separate from a page server and safekeepers. The safekeepers act as Paxos acceptors, and the page server keeps layers in S3. Databricks announced the acquisition on 14 May 2025 for about $1 billion (CNBC). Lakebase became GA on AWS on 3 February 2026 with PostgreSQL 17. There is no peer-reviewed paper.

Forces: Multigres, Neki and Citus are routers in front of PostgreSQL. They inherit PostgreSQL's per-node cost and they add a hop. Document 04 section 4.11 makes rupg's cluster a property of every node, with no router tier. Neon's safekeepers are evidence for a replicated log tier separate from page storage, which is an option for document 18 after M11.

### 1.3.5 What the systems teach together

Every system above solves one or two parts of the goal of document 02 and none solves all of them. The ClickBench rows put the systems that keep PostgreSQL's storage between 569 s and 51,700 s. The systems that replace the storage with a column engine reach 28 s to 61 s. Only engines designed from scratch for analytics reach 5 s to 18 s. No system in that last group shards, runs embedded and passes the PostgreSQL regression suite.

## 1.4 Columnar formats and compression

**FastLanes.** Afroozeh and Boncz, "The FastLanes File Format", PVLDB 18(11), pages 4629 to 4643, VLDB 2025. On 36 Public BI datasets with lightweight encodings only, FastLanes compresses better than BtrBlocks and Parquet with Snappy, and 2 percent better than Parquet with Zstd. It decodes on average 43x faster than Parquet with Snappy, 44x faster than Parquet with Zstd, 7x faster than BtrBlocks and 29x faster than DuckDB (16 ms against 712, 731, 115 and 483 ms). Access to the first value takes 0.14 ms, which is 315x to 800x faster than Parquet and BtrBlocks. AVX-512 adds about 40 percent to decoding. Dictionary encoding is the largest single gain, at about 40 percent. The v0.1 encoder is about 14x slower than Parquet with Snappy (81 s against 5.9 s).

Forces: document 09 uses cascades of data-parallel lightweight encodings in vectors of 1024 values, and no block compressor on the hot path. The 1024 vector of document 04 comes from here. The slow encoder is a warning for the load target of document 02 section 2.4.8. Document 09 must budget encoder speed as a first-class number.

**F3.** Zeng, Meng, Prammer, McKinney, Patel, Pavlo and H. Zhang, "F3: The Open-Source Data File Format for the Future", PACMMOD 3(4), SIGMOD 2025. Decoders embedded as WebAssembly run 15 to 35 percent slower than native code for integers and floats, and up to 46 percent slower for strings. End-to-end scans slow down by 9.6 to 25 percent. Metadata parsing stays near 10 ms with thousands of columns, about 10x faster than Parquet. A checksum for each I/O unit costs almost nothing.

Forces: document 08 uses metadata that does not grow in parse cost with the number of columns, and a checksum on every page and extent. Document 09 does not ship decoders as WebAssembly in the file.

**AnyBlox.** Gienieczko, Kuschewski, Neumann, Leis and Giceva, "AnyBlox: A Framework for Self-Decoding Datasets", PVLDB 18(11), pages 4017 to 4031, VLDB 2025, best research paper. Native SSE2 decoding is about 40 percent faster than WebAssembly SIMD, and WebAssembly SIMD is 2.5x faster than scalar WebAssembly. FSST in WebAssembly loses more than 43 percent. Throughput is about 10 percent below the native Rust Parquet reader.

Forces: the same as F3. Native kernels on the hot path. In the WebAssembly build of document 17, the kernels use 128-bit WebAssembly SIMD, never the scalar fallback, because the scalar path is 2.5x slower.

**BtrBlocks.** Kuschewski, Sauerwein, Alhomssi and Leis, PACMMOD 1(2), SIGMOD 2023. Scans are 2.2x faster and 1.8x cheaper than Parquet on the five largest Public BI datasets. It chooses among eight schemes by sampling and cascades them.

Forces: the encoder of document 09 chooses an encoding for each column chunk from a sample, at write time, and it can cascade.

**ALP.** Afroozeh, Kuffó and Boncz, "ALP: Adaptive Lossless floating-Point Compression", PACMMOD 1(4), SIGMOD 2024, best artifact. Compression and decompression are one to two orders of magnitude faster than other floating-point schemes. DuckDB pull request 9635 reports an average ratio of 4.3x against 2.1x for Patas and 2.4x for Chimp. G-ALP (DaMoN 2025) carries it to GPUs.

Forces: ALP is the default encoding for `real`, `double precision` and decimal-like `numeric` columns in document 09.

**OptFSST.** Chehaidar, Stoian, Stargalla and Kipf, ADMS 2026, arXiv 2607.11271. On 92 datasets the compression factor improves by 7.3 percent for FSST, up to 47.7 percent, and by 17.0 percent for FSST12, up to 91.5 percent. The decoder is unchanged.

Forces: the FSST symbol table builder of document 09 uses the OptFSST construction. The gain is on the write side only, so the read kernels lifted from rudb do not change.

**Lance v2.1.** Pace and others, arXiv 2504.15247, 2025. Parquet tuned correctly gives more than 60x better random access than its defaults. Lance targets at most one I/O for each random access to a fixed-width value and two for a variable-width value, with compression close to Parquet.

Forces: rupg serves point reads of cold rows, so document 10 must reach a cold row in at most two page reads after the summaries are cached: one for the segment directory and one for the vector that holds the value. Document 09 keeps the encodings of a vector independent of other vectors so that one vector can be decoded alone.

**Vortex.** A Linux Foundation project (LF AI and Data) since August 2025, Apache-2.0, and a DuckDB core extension since January 2026 (duckdb.org/2026/01/23/duckdb-vortex-extension). The project claims 10x to 20x faster scans, 100x to 200x faster random access and 5x to 10x faster writes than Parquet. The repository itself says 2x to 10x for scans. All of these are VENDOR. On ClickBench `c6a.4xlarge`, DuckDB with Vortex runs at 40.99 s hot against 26.25 s for DuckDB's own format (`../2140/03-baselines.md` section 3.2).

Forces: a good compressed format does not make a fast engine by itself. The engine must run on the encodings, which document 14 requires. Document 20 compares rupg's file size with Vortex as a sanity check, not only with Parquet.

**DuckDB storage.** DuckDB 1.2 (February 2025) added `DICT_FSST`, Roaring validity masks and Zstd for strings, behind the storage version (duckdb.org/docs/current/internals/storage). The DuckDB file has a 4 KiB main header, two 4 KiB database headers that alternate, and 256 KiB blocks with an 8-byte checksum each. Encrypted blocks have a 40-byte header with AES-GCM (duckdb.org blog, 19 November 2025). The file needs a separate `.wal` file between checkpoints. There is no official byte-level specification, so the details are UNVERIFIED in that sense.

Forces: DuckDB already uses lightweight encodings, so the disk axis cannot reach 10x against it (document 02 section 2.6.3). The `.wal` sidecar is why document 08 puts the log inside the file.

## 1.5 HTAP and OLTP storage

**Colibri.** Schmidt, Durner, Leis and Neumann, "Two Birds With One Stone: Designing a Hybrid Cloud Storage Engine for HTAP", PVLDB 17(11), pages 3290 to 3303, 2024. Up to 10x faster on hybrid workloads, on SSD and on object storage. Hot data is in uncompressed PAX pages under a buffer manager. Cold data is in compressed blocks of up to 262,144 rows. On TPC-H at SF1000 the blocks used 354 GB and the buffer pages only 41 MB.

Forces: this is the blueprint of document 10. The hot store is a B+tree of row pages and the cold store is immutable compressed segments, in the same table. The segment size of 262,144 rows in document 00 comes from here.

**Scalable and robust snapshot isolation.** Alhomssi and Leis, "Scalable and Robust Snapshot Isolation for High-Performance Storage Engines", PVLDB 16(6), pages 1426 to 1438, 2023. In PostgreSQL, InnoDB, WiredTiger and others, TPC-C throughput collapses while one long analytic query runs, because version cleanup waits for the oldest snapshot. The fixes are an ordered snapshot isolation commit protocol, a graveyard index for deleted rows and version storage that adapts.

Forces: document 11 removes old versions without waiting for the oldest snapshot. This is a requirement of the mixed run in document 20, where TPC-C and ClickBench run at the same time.

**What modern NVMe storage can do.** Haas and Leis, PVLDB 16(9), pages 2090 to 2102, 2023. Eight SSDs give 12.5 million random reads per second, which needs about 3,000 I/O requests in flight. LeanStore reaches more than 1 million TPC-C transactions per second on a 4 TB data set with a 400 GB buffer pool, and 0.4 million at 20 TB. PostgreSQL and RocksDB reach a fraction of that.

Forces: the I/O layer of document 08 is asynchronous with deep queues (`io_uring` on Linux), and a session waits on I/O as a task, not as a thread (document 06). The TPC-C target of document 02 section 2.7.2 is inside what LeanStore measured.

**vmcache.** Leis, Alhomssi, Ziegler, Loeck and Dietrich, "Virtual-Memory Assisted Buffer Management", PACMMOD 1(1), SIGMOD 2023. An optimistic read through the buffer manager costs less than 8 percent more than a plain memory read. Scaling past memory needs the exmap kernel module. The LeanStore overview (Leis and others, PVLDB 17, 2024) says incremental checkpoints write about 1 percent of the buffer pool for each GB of log.

Forces: document 08 section 8.8 builds the buffer manager on reserved virtual memory with optimistic version checks. The library and the WebAssembly build cannot require a kernel module, so the exmap path is optional and there is a hash table fallback.

**How to write to SSDs.** Lee, Ziegler and Leis, PVLDB 19(7), pages 1469 to 1483, 2026, arXiv 2603.09927. An in-place update of a 4 KiB page costs 18.85 KiB of flash writes, which is 4.7x. Writing out of place gives 1.65x to 2.24x throughput and 6.2x to 9.8x fewer flash writes on YCSB-A. TPC-C with 15,000 warehouses gives 2.45x throughput and 7.2x fewer flash writes.

Forces: document 08 section 8.6 writes pages out of place and makes a checkpoint reachable through the header slots. The same rule gives atomic commit of the page table, so one design pays twice.

**Moving on from group commit.** Nguyen, Alhomssi, Ziegler and Leis, PACMMOD 3(3), SIGMOD 2025, doi 10.1145/3725328. Each worker has its own log and flushes it independently, and commits are acknowledged in parallel. The throughput and latency numbers are UNVERIFIED, because we could not read the full paper.

Forces: the per-worker log rings of document 11. The design is settled in document 00 on the strength of the mechanism. The gate in document 20 measures the commit latency at the p99 so that the missing numbers are replaced by our own.

**BtrLog.** Kuschewski, Nguyen, Jasny, Ziegler, Leis and El-Hindi, VLDB 2026, arXiv 2606.27051. The log is replicated by quorum to SSD log nodes in one round trip and archived to S3 in the background, with lower latency than EBS. Exact numbers are UNVERIFIED. Milliscale (Zhou and others, arXiv 2603.02108) commits OLTP transactions on S3 Express One Zone and gives no numbers in the abstract.

Forces: document 18 keeps the option of a separate log tier for the cluster after M11. The single-node log format of M1 must allow the log records of a shard to be shipped without the pages.

**Cloudspecs.** Steinert, Kuschewski and Leis, CIDR 2026. From 2015 to 2025, network bandwidth per dollar rose about 10x, while CPU and DRAM improved much less. Instance NVMe has not improved since the i3 generation of 2016. Small providers are up to 5x cheaper, and AWS egress per byte costs 50x Hetzner's.

Forces: in a cluster the network is the cheap resource. Document 18 ships compressed vectors between nodes and does not try to avoid the network at the cost of CPU. On one node, the file must not assume fast local NVMe, because the ClickBench machine uses EBS gp2.

**io_uring for databases.** Jasny, El-Hindi, Ziegler, Leis and Binnig, arXiv 2512.04859. Integrating `io_uring` into PostgreSQL gave 14 percent. SSD-iq (Haas and others, PVLDB 18, 2025) is related and we did not take a number from it.

Forces: `io_uring` is a moderate gain in PostgreSQL because PostgreSQL's process model limits the queue depth. In rupg each worker owns a ring and many sessions share it (document 04 section 4.5), which is the design where deep queues pay.

## 1.6 Distributed transactions and sharding

**Mako.** Shen, Cui, Sen, Angel and Mu, OSDI 2025, pages 129 to 152. Speculative two-phase commit that is decoupled from replication. 3.66 million TPC-C transactions per second on 10 shards with 24 threads each, 8.6x the previous best.

Forces: document 18 keeps the replication round out of the critical path of two-phase commit. The prepare records go to the Raft logs of the shards, and the commit decision does not wait for one more replication round.

**Tiga.** Geng, Mu, Sivaraman and Prabhakar, SOSP 2025, arXiv 2509.05759. Commits in one wide-area round trip by assigning future timestamps from synchronized clocks. 1.3x to 7.2x throughput and 1.4x to 4.6x lower latency.

Forces: the clock layer of document 04 exposes a clock bound from the first milestone. AWS Time Sync and PTP give a bound, and the hybrid logical clock uses it when it exists.

**Detock.** Nguyen, Miller and Abadi, PACMMOD 1(2), SIGMOD 2023. Up to 5x lower latency and about 10x more throughput than other geo-replicated systems under contention. It supports only one-shot transactions. A later evaluation, "Missing Dimensions in Geo-Distributed DB Evaluation" (arXiv 2605.30156), found that SLOG and Detock reach about 100,000 YCSB transactions per second before the p99 passes 10 s, and move about 2x more data.

Forces: deterministic databases need the read and write set before the transaction starts. A PostgreSQL session is interactive and does not know it. Section 1.10 rejects this design.

**Chardonnay.** Eldeeb, Xie, Bernstein, Cidon and Yang, OSDI 2023, pages 343 to 360. Two-phase commit over Paxos takes about 150 µs in one Azure datacenter. A dry run fetches the data before the locks are taken. Chablis (CIDR 2024) follows it.

Forces: in one datacenter with fast RPC, two-phase commit is cheap. What costs is the time that a contended lock is held. Document 18 takes row locks late, after reads, where the statement allows it.

**Accord.** Elliott Smith, Zhang, Eggleston and Andreas, the Cassandra CEP-15 whitepaper, 2021. Leaderless, with one wide-area round trip on the fast path, shipping in Cassandra 6. There are no peer-reviewed benchmarks, so its performance is UNVERIFIED.

Forces: none now. Document 24 keeps a leaderless commit protocol as an option for geo-distributed clusters after M11.

**Redset.** van Renen and others, "Why TPC Is Not Enough: An Analysis of the Amazon Redshift Fleet", PVLDB 17(11), pages 3694 to 3706, 2024. Real fleets have many write-heavy pipelines, many repeated queries and long-tailed data sizes. `SELECT` is 48.9 percent of queries. The claim that more than 99 percent of queries read less than 10 TB came from a secondary source and is UNVERIFIED.

Forces: the plan cache and the summary maintenance of documents 06 and 12 matter as much as cold query speed. The summaries of document 02 section 2.4.5 must be maintained under continuous writes, which the honesty rule already requires.

## 1.7 Execution and optimization

**Bespoke OLAP.** Wehrstein, Eckmann, Jasny and Binnig, VLDB 2026, arXiv 2603.02001. A language model generates a query engine specialized to one workload. Single-threaded on TPC-H at SF20 it is 11.17x faster than DuckDB 1.4.1 and 7.24x faster than Umbra. On CEB it is 45.33x and 9.56x. Multi-threaded on TPC-H it is 7.65x and 6.12x. The ablation is the important part. A flat layout with specialized code gave 1.26x on TPC-H and 0.57x on CEB. A specialized layout gave 12.35x and 51.40x.

Forces: the gain is in the layout. Document 02 section 2.4.6 puts the remaining gap on ClickBench in storage and summaries, and documents 08 to 12 are the longest in the folder for this reason. The drop from 11.17x single-threaded to 7.65x multi-threaded means that about a third of a layout gain can be lost to parallel overheads, so document 14 measures each lever at 16 threads, not at one.

**Sirius.** Yogatama, Yang and others with McKinney and Yu, "Rethinking Analytical Processing in the GPU Era", CIDR 2026, arXiv 2508.04701. A GPU engine that plugs into DuckDB. It reports 7.4x better cost efficiency than DuckDB on ClickBench and 8.3x on TPC-H, and up to 12.5x faster with Doris. It had the lowest ClickBench relative runtime at publication, 11 percent faster than Umbra, on a GH200 at $1.5 per hour. Its current rank on the board is UNVERIFIED.

Forces: section 1.10 rejects a GPU in the core. Document 14 keeps the exchange boundary narrow so that an offload crate stays possible.

**Robust predicate transfer.** Zhao, Su, Yang, Yu, Koutris and H. Zhang, "Debunking the Myth of Join Ordering: Toward Robust SQL Analytics", PACMMOD 3(3), SIGMOD 2025, arXiv 2502.15181. Over random join orders of acyclic queries, the worst run is only 1.6x the best. The geometric mean speedup in DuckDB is about 1.5x. Building and probing the Bloom filters costs 28, 12 and 46 percent of the run time on TPC-H, JOB and TPC-DS. RPT+ (VLDB 2026) gives 1.47x on JOB and 1.17x on TPC-H against DuckDB. Yannakakis+ (PACMMOD 3(3), 2025) cuts one DuckDB TPC-H SF100 case from 488 s to 13.2 s. Shredded Yannakakis (PVLDB 18(8), 2025) is faster on 85.3 percent of 1,849 queries, by up to 62.5x. A CIDR 2026 paper argues that SQL Server's bitmap filters already do this.

Forces: document 15 makes the joins robust in the executor with semi-join reduction and predicate transfer. The filters cost up to 46 percent of run time, so document 14 uses exact bitmaps on row ids where the link index of document 13 exists, and Bloom filters only where it does not. rudb's JOB result of 10.73x (`../2140/bench/job/17-what-the-runs-taught.md` section 17.72) came from exact semi-join reduction with links, not from Bloom filters.

**Unchained hash tables.** Birler, Schmidt, Fent and Neumann, "Simple, Efficient, and Robust Hash Tables for Join Processing", DaMoN 2024, best paper. 2x faster on average than open addressing and up to 20x on graph-shaped queries. Diamond Hardened Joins (Birler, Kemper and Neumann, PVLDB 17, 2024) gives up to 500x on the CE benchmark by splitting a join into lookup and expand steps.

Forces: the default hash join of document 14 is unchained, with lookup and expand steps for joins that multiply rows.

**Adaptive factorization.** Groß, ten Wolde and Boncz, "Adaptive Factorization Using Linear-Chained Hash Tables", CIDR 2025. Factorized aggregation is 1.25x faster, or 17.58x with caching. Worst-case optimal joins range from 0.23x to 14.91x against binary joins on triangle queries, so the choice must be made at run time with HLL and AMS sketches.

Forces: the graph queries of document 13 choose between binary joins and worst-case optimal joins at run time, from sketches, and never by a fixed rule.

**Unified memory and spilling.** Kuiper, Groß, Boncz and Mühleisen, "Saving Private Hash Join", PVLDB 18(8), pages 2748 to 2760, 2025. Performance degrades smoothly past the memory limit with one buffer pool for caches and operators. It shipped in DuckDB 1.2.

Forces: the one memory budget of document 04 section 4.6 and document 19.

**Data chunk compaction.** Qiao and Zhang, SIGMOD 2025, doi 10.1145/3709676. 39 percent of DuckDB's chunks on JOB hold only one row, and compacting them gives up to 63 percent speedup.

Forces: the executor of document 14 compacts sparse vectors after selective filters and joins.

**Compile latency.** Kersten, Leis and Neumann, "Tidy Tuples and Flying Start", VLDB Journal 30, 2021, switch adaptively between a fast backend and LLVM. InkFuse (Wagner, Kohn, Boncz and Leis, ICDE 2024) reports that Umbra's LLVM path still needs about 20 ms per query. TPDE (arXiv 2505.22610) compiles 8x to 24x faster than LLVM at `-O0`.

Forces: the average ClickBench budget is 40.8 ms per query (document 02 section 2.4.2), so a 20 ms compile is half the budget. Document 14 runs vectorized and interpreted first and compiles only pipelines that the plan cache shows are hot or long.

**Learned optimizers.** Lehmann and others (PVLDB 2024) found that PostgreSQL beats learned optimizers in almost all experiments. "How Good are Learned Cost Models, Really?" (SIGMOD 2025) found that the traditional model often wins. CardBench (arXiv 2408.16170) found that zero-shot q-error grows up to 20x on joins. Leis and others, "Still Asking: How Good Are Query Optimizers, Really?", PVLDB 18(12), 2025, is the current review. optd, a Cascades optimizer in Rust from CMU, has a blog post and no paper.

Forces: no learned component in the optimizer of document 15. Section 1.10 records the rejection.

## 1.8 Graph and vector indexes

**DiskANN in Azure Cosmos DB.** Upreti and others, PVLDB 18(12), pages 5166 to 5183, 2025, arXiv 2505.05885. Less than 20 ms latency at 10 million vectors. Query cost about 43x lower than Pinecone serverless and 12x lower than Zilliz serverless. There is one index for each partition, stored in the database's own Bw-tree.

Forces: document 13 stores the vector graph in rupg's own pages under the same log, MVCC and shards as the table. There is no separate index file.

**RaBitQ and extended RaBitQ.** Gao and Long, PACMMOD 2(3), SIGMOD 2024, doi 10.1145/3654970. The error is bounded and RaBitQ beats product quantization. Extended RaBitQ (Gao and Long, SIGMOD 2025, arXiv 2409.09913) has asymptotically optimal error, and at 2 bits per dimension it is orders of magnitude more accurate than scalar quantization. Weaviate measured 4,000 µs or more to encode one 1536-dimension vector at 8 bits (VENDOR).

Forces: the vector graph of document 13 keeps RaBitQ codes in the graph pages and the exact vectors in the table for re-ranking. The encode cost goes to the mover, not to the `INSERT` statement.

**ACORN and filtered search.** Patel, Kraft, Guestrin and Zaharia, SIGMOD 2024, doi 10.1145/3654923. 2x to 1,000x more queries per second at fixed recall, and more than 1,000x on a 25 million vector data set, against filtering before or after the search. Building ACORN-γ is 8.8x to 33.1x slower than HNSW. A later paper reports that ACORN-1 recall drops to 0.45 to 0.72 at 0.3 to 1 percent selectivity. Lu, Caminal, Özcan and others, SIGMOD 2026, arXiv 2603.23710, found that inside PostgreSQL, graph methods test the filter too often and clustering methods in the style of ScaNN can win.

Forces: the optimizer of document 15 chooses by cost between filtering before the search, during it and after it, from the selectivity of the filter. No single strategy wins.

**Range-filtered search.** SeRF (Zuo and others, SIGMOD 2024) and iRangeGraph (Xu and others, PACMMOD 2(6), SIGMOD 2025) do not support updates.

Forces: rupg's tables change, so document 13 does not adopt either structure.

**pgvectorscale.** StreamingDiskANN for PostgreSQL. The vendor reports 28 ms p95 against 784 ms for Pinecone s1 at 99 percent recall on 50 million Cohere vectors of 768 dimensions, and 16x the queries per second. This is VENDOR.

Forces: pgvector with pgvectorscale is the PostgreSQL extension that document 13 must match on recall and beat on CPU per query, measured by us.

**DuckPGQ.** ten Wolde, Singh, Szárnyas and Boncz, CIDR 2023, and a PVLDB 16(12) demonstration, 2023. SQL/PGQ inside DuckDB, with CSR adjacency built on the fly and multi-source BFS. A thesis reports up to 11.46x on shortest paths at LDBC SNB SF300.

Forces: document 13 stores the CSR adjacency as a persistent index (the link index) instead of building it for each query, and it runs graph traversal over that index.

**Kuzu.** Apple agreed to acquire Kuzu on 9 October 2025, and the repository was archived on 10 October 2025 at v0.11.3. LadybugDB and Bighorn are forks.

Forces: there is no maintained embeddable graph engine to take code from. The graph work of document 13 lifts from rudb's link graph (`../2140/graph/`) instead.

## 1.9 Embedded and single-file databases

**SQLite.** Gaffney, Prammer, Brasfield, Hipp, Kennedy and Patel, "SQLite: Past, Present, and Future", PVLDB 15(12), pages 3535 to 3547, 2022. It is older than the window of this document, but it is the reference result. SQLite beats DuckDB by 10x to 500x on write transactions on a cloud server and by 2x to 60x on a Raspberry Pi. DuckDB beats SQLite by 3x to 50x on SSB. A Bloom filter technique made SQLite 4.2x faster on SSB, and it shipped in 3.38.0. SQLite commits through a rollback journal or a WAL file, both sidecar files. Page checksums are an optional VFS shim (sqlite.org/cksumvfs.html). `BEGIN CONCURRENT` lives on a branch and is not in the main release.

Forces: neither engine wins both workloads, which is the gap that rupg's hybrid table is for. Document 20 measures the library mode of rupg against SQLite on the YCSB point path in process, as a sanity row, and against DuckDB on ClickBench in process. Document 08 makes page checksums mandatory and document 11 makes concurrent writers the default.

**DuckDB single file.** See section 1.4. The two alternating headers and the block checksums are the same pattern that rupg uses. The `.wal` sidecar is the difference.

**LMDB.** A copy-on-write B+tree with two meta pages that alternate as the commit point, one writer, and zero-copy reads through `mmap`. There is no write-ahead log. This is background and was not checked again this session.

Forces: the two header slots of document 08 are the LMDB and DuckDB pattern. LMDB's reliance on `mmap` for I/O is not taken, because it gives up control of I/O order and depth, which section 1.5 needs.

## 1.10 What we rejected, and why

**A deterministic database.** Calvin, SLOG and Detock need the read and write set of a transaction before it runs. A PostgreSQL session sends statements one at a time and decides the next one from the last answer. The evaluation in arXiv 2605.30156 also found that these systems stop near 100,000 YCSB transactions per second at a 10 s p99. Document 18 uses timestamps and two-phase commit.

**A GPU in the core.** Sirius shows the cost advantage. rupg must run embedded on a laptop and in WebAssembly, where there is no GPU. Document 14 keeps the exchange boundary that an offload crate would use.

**WebAssembly decoders in the file.** F3 and AnyBlox both measured a loss of 10 to 46 percent against native kernels. rupg writes its own files and has no need to decode foreign encodings on the hot path.

**A learned optimizer.** Section 1.7 lists four results that found the traditional approach as good or better. Robustness comes from the executor.

**A key-value layer under the tables.** CockroachDB and YugabyteDB are the slowest systems on the ClickBench board, at 10,965.71 s and 51,701.64 s. A key-value layer turns every column read into a key decode. Document 10 stores rows and columns natively and shards them as they are.

**A router tier in front of PostgreSQL.** Citus, Multigres and Neki keep PostgreSQL's per-node cost, which is axis 3, and add a hop, which is axis 4. Document 04 section 4.11 puts the cluster inside every node.

**A second in-memory column copy.** AlloyDB keeps one. rupg keeps each row in one place, because axis 3 counts the memory.

**Running PostgreSQL C extensions.** It requires the internal C functions of PostgreSQL, which rupg does not have. Document 16 implements the most used extensions natively.

**Reading AGPL code.** pgrust, ParadeDB, PgDog and Hydra are AGPL. The clean room rule of document 00 applies.

## 1.11 Gaps in this research

The following are open and each one has an owner. The throughput and latency numbers of "Moving on From Group Commit" are UNVERIFIED, and document 20 replaces them with our own commit latency measurement at M1. BtrLog's numbers are UNVERIFIED and matter only after M11. Accord's performance has no peer-reviewed number. CedarDB and Neon have no papers. The ClickBench result files do not record the PostgreSQL, DuckDB or ClickHouse version, so document 03 treats the published versions as UNVERIFIED and document 20 records the versions of our own runs. "Taming the WAL" was searched for and not found, and it is not cited.
