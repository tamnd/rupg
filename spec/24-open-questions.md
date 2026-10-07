# 24. Open questions

Written 7 October 2026.

This document is the register of questions that the other documents could not close. Each question has a number, the document that raised it, the milestone that must close it, and the thing that decides it. Most questions close with a measurement. A few close with a decision that the project owner must make. Sections 5.14, 6.20, 7.20, 12.13, 13.9, 14.17, 15.14 and 16.16 keep the local form of some questions. This register is the master copy, and a question closes here first.

## 24.1 How to use this register

**A question closes with evidence.** A measurement closes a question when it follows the rules of document 20 and its report is in `rupg-bench`. A decision closes a question when the decision and its reason are written in the document that owns the topic. The register then moves the question to section 24.2 with one line that names the evidence.

**A question does not block work unless it says so.** The milestone column says when the answer must exist. Before that milestone, the documents use the default that they state, and the code must make the default easy to change.

**A new question needs an owner.** A question that a later run raises goes into the section of its area with the next free number. Numbers are not reused.

## 24.2 Closed while the documents were written

These questions came up while the documents were written. They are closed, and the decision is in the owning document.

**C1. Synopses never supply grouped output.** Document 02 H3 first said that a certified top 10 is exact without a scan. `../2140/storage-v3/34-no-query-answer-blocks.md` found that a stored top-k answer is a query answer in the file. H3 and document 12 section 12.8.4 now agree: the synopsis narrows the rows read, and the query computes groups and counts when it runs. `rupg.synopsis_mode` takes `count` or `off` only.

**C2. The extension machinery arrives at M6.** Documents 12 and 13 need `btree_gist`, `pg_trgm` and the pgvector surface at M6. Documents 22 and 23 now place `rupg-ext` and the first ports in `rupg-contrib` at M6. The rest of the contrib ports stay at M7.

**C3. The mover is split between two layers.** The physical move of rows is in layer 4 (`rupg-table`). The policy and the transaction that swaps the rows are in layer 5 (`rupg-txn`), because the move needs the visibility rules. Documents 04 and 22 agree.

**C4. Six crates may contain `unsafe`.** Document 04 section 4.8 now lists the WebAssembly bindings with the platform crate, the buffer manager, the kernels, the compile tier and the C API.

**C5. No uncommitted version reaches the disk.** The page writer rolls back uncommitted versions in its copy of a page (document 11). Document 08 now says that the page timestamp is the newest visible commit timestamp at the time of the copy, and that recovery drops the log blocks of transactions without a durable commit instead of an undo pass.

**C6. Hidden `xmin` and `xmax` minipages.** Document 11 keeps the 16-byte version header of document 10 and stores the 32-bit `xmin` in a hidden minipage in each hot leaf, with an `xmax` minipage only when a row in the leaf has a deleter or a locker. Document 10 section 10.3.1 lists both, and the cost per row is 28 bytes against PostgreSQL's 27.

**C7. G11 runs at replication factor 1.** At factor 3, follower apply limits capacity to about 0.77 of one node (a budget in document 18), which is below 0.8 N. The gate compares distribution with one node, so both sides run at factor 1. A row at factor 3 is published beside it. Document 02 section 2.8 states this.

**C8. The seven G1 clients.** They are psql, pgAdmin 4, DBeaver, psycopg 3, node-postgres, pgx and asyncpg. Document 02 section 2.10 names them.

**C9. A first SQL path ships at M2.** G1 needs clients to run catalog queries. The parser, a first analyzer and a simple executor therefore ship at M2, and M3 completes them (document 23).

**C10. `GROUP BY ALL` is not in PostgreSQL 19.** `gram.y` at the pin, 20,077 lines, has no `GROUP BY ALL` production. `ALL` after `GROUP BY` is only the standard set quantifier. rupg does not add the feature (document 05).

**C11. The ClickBench sort key comes from the data.** The plain `postgresql/create.sql` of ClickBench at `e6bda4e` declares no primary key. The rupg entry uses that schema, so the data rule of document 10 section 10.7.1 chooses the sort key.

## 24.3 The 10x targets

**Q1. Do the ClickBench rules accept the summaries?** H1 to H6 build structures at load from the data, under the honesty rule of document 02 section 2.4.5. The ClickBench maintainers decide what the untuned category allows. Raised by documents 02 and 20. Closes before the first submission, after M8. Decided by a written answer from the maintainers. Until then, every published rupg row has a second row with `rupg.use_summaries = off`.

**Q2. The class budgets against the best published times.** Document 03 found that C1, C2, C3 and C7 are 6.4x, 3.7x, 4.3x and 2.2x over their budgets even when each query takes the best time that any system has published. The excess is 3.15 s, and C3 is 73 percent of it. The reserve of 268 ms cannot absorb a class that misses. Raised by document 03. Closes at M8. Decided by the per-class measurement at M4 and the rebalancing rule: a class may borrow from the reserve and from a class that is under budget, but the total stays 1,753 ms.

**Q3. Exact distinct count on a column without a dictionary.** Q4 is `count(DISTINCT UserID)` on a `BIGINT` with about 17.6 million distinct values. The best published time is 58 ms against a budget of 2 ms. Per-segment code sets do not answer it without a merge of all of them. Q8, Q9 and Q13 have the same problem inside groups. The choices are a table-level exact distinct structure maintained under writes, or a new budget. Raised by document 03. Closes at M8. Decided by a prototype at M4 that reports the bytes and the update cost of the structure.

**Q4. Does count mode meet the C3 budget?** Document 12 section 12.8.4 predicts 1 to 2 ns per row for the membership test of count mode. The first M8 measurement runs each C3 query and reports whether the certificate held. A query whose certificate fails needs another mechanism. Raised by document 12. Closes at M8.

**Q5. Peak memory on Q32.** A plain hash aggregate over about 100 million groups needs 4.4 GB or more, which is above the 2 GiB design budget of document 19. The choices are certification by H3 or a hash-partitioned aggregate that reads the input more than once. Raised by document 19. Closes at M8.

**Q6. G8 against small baseline peaks.** The 2 GiB budget gives 10x only if each baseline peaks above 20 GiB. If ClickHouse peaks lower on `c6a.4xlarge`, G8 needs its own design work. Raised by document 19. Closes at M0, when the baselines are measured.

**Q7. The disk gate G3.** rudb stored `hits` in 9.65 GB, which is 0.2 GB above the M4 gate of 9.45 GB, and the summaries add bytes. No measured saving covers the gap yet. The log rings take 1 GiB at 64 MiB per worker on 16 workers unless the ClickBench run uses 8 MiB per worker. Raised by documents 09 and 19. Closes at M4.

**Q8. Encoder speed.** The load target needs about 18 times rudb's encoder speed. rudb took 5.6 CPU hours for the table and never measured where the time went. The step budget of document 09 section 9.4 is a plan. Raised by document 09. Closes at M4, with a profile of each encoder step.

**Q9. Load sort in runs.** A load sorts in runs of 16 segments, not globally. Per-segment bounds overlap across runs until compaction merges them. It is not known whether this is enough for the cold target of 11.37 s. Raised by document 10. Closes at M4.

**Q10. The load source before M9.** PostgreSQL loads `hits.tsv` and DuckDB loads `hits.parquet`. rupg has no Parquet reader until the importer of M9. Raised by document 20. Closes at M4. The default is TSV through `COPY`, with the source read time of each format in the report.

**Q11. Precomputed answers on the board.** One system reports 0.000 s for Q29, which is 90 full-table sums, so it probably computed the answer before the query. This changes how the Best column of document 03 must be read. Raised by document 03. Closes at M0. The default is to keep such rows in the table and leave them out of every ratio.

**Q12. `max_parallel_workers_per_gather`.** PostgreSQL reports 2, and an analytic query on 2 of 16 cores is 8 times slower than it can be. The choices are an effective default that differs from the reported value, or a gate run that sets the value and prints it as a configuration line. Raised by document 19. Closes at M3.

## 24.4 Compatibility

**Q13. Float aggregates in a different order.** A parallel or summary-based float sum can differ from the oracle's serial scan in the last bits. Either document 05 adds an exclusion for the last bits of `float4` and `float8` aggregates, or rupg matches PostgreSQL's serial order, which forbids parallel float sums. Raised by documents 09 and 21. Closes at M3.

**Q14. ctid that does not change.** The ctid of document 10 section 10.11.1 is computed from the row id and stays the same across `UPDATE` and `VACUUM FULL`. A program that detects an update by a changed ctid, or that estimates bloat from ctid blocks, behaves differently. Raised by document 10. Closes at M7, from the count of regression and client tests that depend on it.

**Q15. Shims and newer syntax.** The grammar is not shimmed, so a database with `rupg.compat_version = 14` still parses syntax from 19. Raised by document 05. Closes at M9. The default is to accept the newer syntax.

**Q16. The size of the shim tables.** The catalog and setting tables for versions 14 to 18 add bytes to the binary. They may need a cargo feature that is off in the library and WebAssembly builds. Raised by document 05. Closes at M9.

**Q17. When version 14 leaves the shim.** PostgreSQL 14 reaches end of life on 12 November 2026. The choices are removal at end of life or one year later. Raised by document 05. Closes at M9.

**Q18. Collation order of ICU4X against ICU4C.** The oracle uses ICU4C. rupg uses ICU4X. The order must match for every locale that `initdb` imports at the same CLDR version. Libc collations other than C and POSIX map to the nearest ICU collation. Raised by documents 07 and 22. Closes at M3, by a corpus test.

**Q19. Server encodings.** rupg accepts UTF8 and SQL_ASCII and refuses the others with `0A000`. Raised by document 07. Closes at M7, from the client matrix.

**Q20. Math functions on other platforms.** The oracle's `sin`, `exp` and `pow` give glibc's answers. macOS, Windows and wasm32 can differ in the last bit. The choice is to ship correctly rounded functions in Rust. Raised by document 22. Closes at M3.

**Q21. Hash values that users can see.** `hashint4`, `hashtext` and hash partitioning must give PostgreSQL's values. The hashes lifted from rudb differ, so `rupg-kernels` keeps two sets. Raised by document 22. Closes at M3.

**Q22. The TAP subset.** Most of the 294 TAP files test PostgreSQL's own programs and data directory. Document 05 must list the files that apply. Raised by document 21. Closes at M7.

**Q23. `cmin`, `cmax` and the SSI expected files.** `cmin` and `cmax` show 0 for rows of other transactions. The isolation specs `predicate-gin`, `predicate-gist` and `predicate-hash` are expected to differ, because rupg's predicate locks are not page locks. The choice of victim in SSI may differ from PostgreSQL's in other specs. Raised by document 11. Closes at M5, when the real list is known.

**Q24. The B-tree key limit.** PostgreSQL refuses keys over 2,704 bytes with `54000`. rupg's 16 KiB pages allow longer keys, but tools and tests may depend on the limit. Raised by document 12. Closes at M6. The default is PostgreSQL's limit.

**Q25. `relpages`.** The choices are bytes divided by 8,192, which makes size arithmetic correct, or a page count chosen so that cost formulas in external tools behave as on PostgreSQL. Raised by document 15. Closes at M3.

**Q26. The `pg_stat_statements` query ID.** PostgreSQL computes the ID by a walk of its query tree. rupg must reproduce the walk on its own tree for every statement, or the IDs differ. Raised by document 15. Closes at M7.

**Q27. The EXPLAIN node names.** `Link Join`, `Certified Top-N` and `Graph Search` are new names. A tool that parses EXPLAIN may fail on them. `rupg.explain_names` may need the default `postgres` for the wire server and `rupg` for the library. Raised by document 15. Closes at M6.

**Q28. `pg_column_compression` on cold values.** Cold values are not compressed one value at a time, so the function returns NULL. Raised by document 09. Closes at M4.

**Q29. Restarts from clock uncertainty.** In a cluster, a `REPEATABLE READ` statement that must restart after it returned rows fails with `40001`, which PostgreSQL never does. The default of 250 ms for `rupg.max_clock_offset` is a budget. Raised by document 18. Closes at M11, from the client suites on a cluster.

## 24.5 The file, storage and compression

**Q30. The log ring size.** Document 19 proposes 64 MiB per worker. Document 08 owns ring sizes and starts each ring at 64 MiB in 16 MiB extents, which is large for an embedded library. Raised by documents 11, 17 and 19. Closes at M1, from the load and TPC-C runs.

**Q31. The checkpoint trigger.** `max_wal_size` applies to each ring, so the worst redo is the number of rings times 1 GB, about 16 s on 16 rings. The other choice is the sum over all rings. Raised by document 11. Closes at M1.

**Q32. Dictionary leftovers.** Strings from deleted rows and aborted transactions stay in the dictionary until `REPACK` or `VACUUM FULL`. A user who deletes data for privacy expects it to be gone. Raised by document 09. Closes at M4. The choice is between a scrub in compaction and a documented rule.

**Q33. Dictionaries across partitions.** Each partition has its own dictionary, so a group across partitions cannot use codes directly. One shared dictionary would fix that, but `ATTACH` and `DETACH` would then rewrite codes. Raised by document 10. Closes at M4.

**Q34. Dictionaries across nodes.** ClickBench on 8 nodes needs a skew factor s(8) of about 2.82 or less, and every C3 certificate must hold. One fallback shuffle costs at least 30.7 ms of the 50.1 ms of slack. The choice is between per-node and cluster-wide dictionaries. Raised by document 18. Closes before M11, by a measurement of s(N).

**Q35. Segments under Raft.** Segment bytes are not in the log. Raft must either ship extents or depend on the encoder being deterministic, so that a follower builds the same bytes. Raised by documents 08 and 10. Closes at M10.

**Q36. The mover policy.** The stable age (60 s by default, up to 24 h) and the minimum hot size of 64 MiB are budgets aimed at TPC-C `stock`. Raised by document 10. Closes at M8.

**Q37. Data key rotation.** A new passphrase only rewraps the data key. A new data key means a rewrite of every page. Raised by document 08. Closes at M10, with backup.

**Q38. 39-bit aarch64 hosts.** On hosts with a 39-bit virtual address space, each buffer window is limited to 128 GiB, so a larger file uses the hash table fallback. Its cost is not measured. Raised by document 08. Closes at M1.

**Q39. FSST with 12-bit codes.** The 17.0 percent gain needs a different decoder. Only the OptFSST trainer with the 8-bit decoder is adopted now. Raised by document 09. Closes at M8.

**Q40. Two files during an upgrade.** Between `0.M` lines, `rupg upgrade` writes a new file, so two files exist while it runs. The rule of no sidecar file applies during operation, and the upgrade is not operation. Raised by document 23. Closes at M1, with the format version rule of document 08.

## 24.6 Transactions and the cluster

**Q41. The wait before CommandComplete.** A read-only statement that saw a version whose commit is not yet durable waits before `CommandComplete`. The wait could move to the first `DataRow`. Raised by document 11. Closes at M5.

**Q42. The LSN as a time.** pg_lsn values are commit timestamps (document 11 section 11.12). A byte count over all rings could serve monitoring tools better. Raised by document 11. Closes at M7, from the tools in the client matrix.

**Q43. Reference tables.** A write to a reference table uses a future timestamp and a commit wait. It is not known if this is the right default for `item` in TPC-C. Raised by document 18. Closes at M11.

**Q44. Unique constraints without the shard key.** The default refuses them in a cluster at M11 and adds global unique indexes at M12. Raised by document 12. Closes at M11.

**Q45. A built-in transaction pooler.** The design has none and depends on PgBouncer compatibility. It predicts about 625 MiB for 10,000 idle connections. Raised by document 06. Closes at M8, from the connection run of document 20.

**Q46. TLS state in the idle budget.** It is not known if the TLS state fits in the 24 KiB that document 06 section 6.4 gives it. kTLS on Linux would move it into the kernel. Raised by document 06. Closes at M2.

**Q47. Session moves and work stealing.** A session moves to another worker only between statements. A long statement on a busy worker may need stealing at the morsel level. Raised by document 06. Closes at M8, with the HTAP run.

## 24.7 Execution and planning

**Q48. The HTAP target as a gate.** Document 14 section 14.11.2 sets a budget for the growth of TPC-C p99 latency while ClickBench runs on the same node. It is not a gate in document 02. Raised by document 14. Closes at M8.

**Q49. Long analytic queries and undo.** A long query could copy the hot rows it reads at its start, so that undo can be released. Raised by document 14. Closes at M8.

**Q50. Regular expressions.** It is not known if a DFA covers enough of PostgreSQL's ARE flavour, or if a backtracking fallback is needed. Raised by document 14. Closes at M7, by the regression tests.

**Q51. The point path on other platforms.** The point path budget of 2.2 µs is for Linux. macOS and WebAssembly are not measured. Raised by document 14. Closes at M9.

**Q52. Planning time for large joins.** `rupg.join_dp_limit` is 14. It is not known if JOB plans in less than 10 ms at that limit. A worst-case optimal join is chosen when it is 4 times better than the best binary plan, and that factor is also a budget. Raised by document 15. Closes at M6.

**Q53. Compaction thresholds.** Vector compaction can use one threshold or one per operator. Raised by document 14. Closes at M8.

**Q54. Thresholds of the summaries.** A column gets a synopsis at 1,024 distinct values. The sparse index form merges when a probe visits more than four segments. Filtered vector search pre-filters below 50,000 rows. All three are budgets. Raised by documents 12 and 13. Closes at M8 for the first two and M6 for the third.

**Q55. Links on hot rows and inferred links.** Hot rows can store the link, 8 bytes per row, or the mover can compute it. Links could also be inferred from the data on schemas with no foreign keys, which may conflict with the honesty rule. Raised by document 13. Closes at M6.

**Q56. The vector target as a gate.** Document 13 targets 10x queries per second per core against pgvector 0.8 at the same recall with half the memory. It is not a gate in document 02. Raised by document 13. Closes at M6.

**Q57. Property graph syntax in PostgreSQL 20.** If PostgreSQL 20 adds SQL/PGQ, rupg follows its syntax and turns `rupg.property_graph` on by default. Raised by document 13. Closes at the pin move to 20.

**Q58. `jit` and the compile tiers.** `jit` is accepted and shown. `rupg.compile` controls the tiers. The other choice is that `jit = off` also turns off the compile tiers. Raised by document 06. Closes at M6.

**Q59. The plan dependency check.** After DDL changes the catalog version, every plan cache hit checks the plan's dependencies. A migration run beside TPC-C decides if this is cheap enough. Raised by document 07. Closes at M7.

## 24.8 Embedding, extensions and platforms

**Q60. A drop-in libpq.** It is not known if the drop-in library needs GSSAPI and the OAuth flow of PostgreSQL 18. Raised by document 17. Closes at M9, from the client matrix.

**Q61. WebAssembly scope.** Open items are a `memory64` build, a WebAssembly compile tier, a WASI build, browser extensions through the browser's own engine, and a cache of compiled extension code in the file. Raised by documents 16, 17 and 19. Closes at M9.

**Q62. Network filesystems.** NFS, SMB and FUSE are refused by default. The 9p filesystem of WSL may need to be on the list. Raised by document 17. Closes at M9.

**Q63. PostGIS.** A port needs a geometry engine in Rust that gives the same answers as GEOS, and the PROJ data. Raised by document 16. Closes after M12.

**Q64. A JavaScript procedural language.** It could ship as a WebAssembly module. Raised by document 16. Closes after M9.

## 24.9 The project

**Q65. A shared kernel crate with rudb.** rupg lifts code from rudb with provenance and has no Cargo dependency on it. A shared crate would stop the two copies from drifting, but it couples the release cycles. Raised by document 22. Closes at M4, after the lifts of M4 show how much changes.

**Q66. Umbra and CedarDB.** If their licenses do not allow a published comparison, their numbers stay as references and no gate uses them. Raised by documents 03 and 20. Closes at M0.

**Q67. The gate machines.** The `oltp` machine needs an fdatasync p50 below 100 µs. TPC-H SF100 needs a larger `tpch` machine. Raised by document 20. Closes at M0.

**Q68. The load case with a warm page cache.** Document 20 section 20.3 runs the load with the source in the page cache and without it. The gate reads the cold case. The board of ClickBench does not say which case each system used. Raised by document 20. Closes at M0.

**Q69. Thresholds of the performance tests.** 3 percent more instructions on one query fails a change, 1 percent in total fails a change, a nightly slowdown over 5 percent opens a bug, and CI must finish in 30 minutes. These are budgets until the noise is measured. Raised by document 21. Closes at M1.

**Q70. Crate names.** All 41 crate names and the two sibling repository names are free today. crates.io does not allow name squatting, so names are published at M1. Raised by document 22. Closes at M1.

**Q71. Dates.** The documents give no dates. A forecast is possible after M2, when the time per milestone and the lines against the budgets of document 22 are measured. Raised by document 23. Closes at M2.

**Q72. Facts not verified.** The numbers in "Moving on From Group Commit" (SIGMOD 2025) are not verified against the paper. The release that added self-join elimination to PostgreSQL is not confirmed. Raised by documents 01 and 15. Closes at M3.
