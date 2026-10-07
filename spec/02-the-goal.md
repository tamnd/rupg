# 2. The goal

Written 7 October 2026. The ClickBench numbers come from the result files of `github.com/ClickHouse/ClickBench` at `e6bda4e` (6 October 2026). The PostgreSQL numbers for TPC-C come from the Small Datum report of 15 February 2026. The rudb numbers come from `rudb-bench/reports/` and from `../2140/` at the paths given in each section. Document 03 has the full baseline tables.

This document states the goal of rupg as five axes. Each axis has a number, a machine, a baseline and a gate. The document also does the arithmetic for each number and says what must be true for the number to hold. Where the published data says a number cannot hold, the document says so, and it gives the number that can.

## 2.1 The requirement

The requirement, as given:

1. rupg is 100 percent compatible with PostgreSQL 19, with a shim for older versions.
2. rupg is embeddable and stores a database in one columnar, compressed file.
3. rupg has graph indexes.
4. rupg is ready for shards and for a cluster.
5. rupg is 10x faster than PostgreSQL, ClickHouse and DuckDB on ClickBench, and it uses 10x fewer resources. TPC-H, TPC-C and other suites follow.

Items 1 to 4 are properties of the design, and the documents 05 to 18 specify them. Item 5 is a measurement. A measurement needs a machine, a metric, a baseline, a transport and a rule for what counts. Sections 2.3 to 2.8 give these for each suite.

## 2.2 How a 10x claim is made

A 10x claim in this folder has five parts. A claim that does not have all five parts is not made.

1. **The machine.** One named machine type. The default for ClickBench is `c6a.4xlarge` (16 vCPU AMD EPYC 7R13, 8 physical cores, 32 GiB, EBS gp2, 500 GB), because most published results use it.
2. **The metric.** One number, defined by the suite. For ClickBench the headline metric is the hot sum: for each of the 43 queries, take the minimum of the three runs, and add the minimums.
3. **The baseline.** A number that we measured on the same machine type, with the baseline system at a named version and a named configuration. Published numbers are a sanity check. They are not a baseline.
4. **The transport.** rupg and the baseline use the same transport: in process for both, a Unix socket for both, or TCP for both.
5. **The ratio.** The baseline number divided by the rupg number. "10x" means the ratio is 10.0 or more against every baseline in the claim, not against the slowest one.

The target is set by the fastest baseline. On ClickBench the fastest of the three named baselines is ClickHouse. A result that is 10x faster than ClickHouse is also more than 10x faster than DuckDB and PostgreSQL, so the target is one tenth of ClickHouse.

## 2.3 Axis 1: compatibility

The levels and denominators are the ones in `../2140/compat/postgres/01-what-compatible-means.md`, with rupg in place of rudb. Document 05 restates them and adds the version shim.

| Level | Denominator at the pin | Gate for "100 percent" |
|---|---|---|
| L1 Connect | protocol traces of 48 clients, the `libpq_pipeline` test program | every trace byte-identical to the oracle, except the fields that document 05 excludes |
| L2 Introspect | 64 catalogs, 86 system views, 65 `information_schema` views, every column | every row and every column equal to the oracle for the same schema |
| L3 Query | the 239 regression tests (290,977 lines of expected output) and the differential corpus | the same pass set as the oracle |
| L4 Behave | the 135 isolation specs and Hermitage | the same outcome as the oracle for every permutation |
| L5 Drop-in | the test suites of the 48 clients in `../2140/compat/postgres/13-clients-and-tools.md` | the same pass rate as against the oracle |

"100 percent" is the gate of milestone M12, not of an earlier one. The earlier milestones have lower gates, in document 23. The version shim has its own denominators: the same suites run against an oracle of each older major version from 14 to 18. Document 05 section 5.6 says which suites run for each version.

## 2.4 Axis 2: analytic speed on ClickBench

### 2.4.1 The number

On `c6a.4xlarge` the hot sums at the pin are:

| System | Hot sum | Source |
|---|---|---|
| PostgreSQL (default configuration) | 11,875.43 s | `postgresql/results/c6a.4xlarge.json`, run of 21 September 2026 |
| PostgreSQL (with indexes, tagged tuned) | 4,085.54 s | run of 10 March 2025 |
| DuckDB | 26.25 s | run of 11 May 2026 |
| ClickHouse | 17.53 s | run of 6 October 2026 |
| Umbra 26.09 | 7.41 s | run of 18 September 2026 |
| elosdb (first on the board) | 5.21 s | run of 2 October 2026 |

The target is 17.53 / 10 = **1.753 s**. That is 4.2x faster than Umbra and 3.0x faster than the first system on the board. No CPU system has published a number within 2.9x of the target on this machine.

On `c6a.metal` (192 vCPU) the same rule gives 4.40 / 10 = 0.440 s against ClickHouse. Umbra is at 3.80 s and elosdb is at 2.93 s. The metal target is a second gate at milestone M8. The `c6a.4xlarge` target is the first gate.

### 2.4.2 The budget per row

1.753 s over 43 queries is 40.8 ms per query on average. The `hits` table has 99,997,497 rows. A query that must look at one value of every row has 0.41 ns of wall time per row. With 16 vCPUs working in parallel, that is 6.5 ns of vCPU time per row. At about 3 GHz, which is an assumption about the clock of the EPYC 7R13 and not a measurement, that is about 20 cycles per row per query.

Twenty cycles is enough to read a bit-packed integer, test it and add it to a sum. It is not enough to hash a string, to look up a hash table that does not fit in cache, or to run a regular expression. So a query that must group or search 100 million strings cannot run on the strings. It must run on dictionary codes, on summaries, or on a small subset of rows that the summaries select. This is the central fact of the design. It is why documents 09, 10 and 12 put summaries in the file and why document 14 runs operators on codes.

### 2.4.3 The budget per query class

The 43 queries fall into nine classes by what they read. The numbering is from 0, as in `../2140/03-baselines.md`. The budget is a requirement that we set. It is not a measurement. The sum is 1,485 ms, which leaves 268 ms, or 15 percent, as reserve.

| Class | Queries | What the query does | Budget each | Budget total |
|---|---|---|---|---|
| C1 whole-table scalars | Q0 to Q6 | `count(*)`, sums, averages, min and max, `count(DISTINCT)` over the whole table | 2 ms | 14 ms |
| C2 grouped, small keys | Q7 to Q14 | group by an engine, a region, a phone model or a search phrase, with a filter | 25 ms | 200 ms |
| C3 grouped, large keys, top-k | Q15 to Q18, Q30 to Q35 | group by user, IP, watch id or URL, order by count, limit 10 | 70 ms | 700 ms |
| C4 point | Q19 | one `UserID` | 1 ms | 1 ms |
| C5 substring search | Q20 to Q23 | `LIKE '%google%'` on URL and Title | 60 ms | 240 ms |
| C6 sort with limit | Q24 to Q26 | order by time or phrase, limit 10 | 30 ms | 90 ms |
| C7 string functions | Q27, Q28 | `length(URL)` per counter, a regular expression on Referer | 100 ms | 200 ms |
| C8 wide arithmetic | Q29 | 90 sums of `ResolutionWidth + k` | 5 ms | 5 ms |
| C9 one counter, date range | Q36 to Q42 | `CounterID = 62` with a date range, the dashboard queries | 5 ms | 35 ms |

Class C3 holds 40 percent of the budget. It is also where the baselines spend most of their time. In the Umbra run that `../2140/02-the-goal.md` section 2.3 analyzed, seven queries took 70.4 percent of the hot sum: Q28 1.393 s, Q32 1.323 s, Q18 0.846 s, Q34 0.732 s, Q33 0.730 s, Q16 0.368 s and Q13 0.309 s. Five of those seven are in C3, one is in C2 and one is in C7.

### 2.4.4 What must be true

The budget holds only if six statements hold. Each statement is a design requirement in a later document, and each one has a measurement in the rudb work or the literature behind it.

**H1. Scalars come from segment summaries.** Each segment stores, for each column, the row count, the null count, the minimum, the maximum and the sum, and for each 1024-value vector the minimum and the maximum. `count(*)`, `sum`, `avg`, `min` and `max` over the whole table, and over a filter that a whole segment passes, read the summaries and no values. `count(DISTINCT)` on a dictionary-coded column reads the set of codes present in each segment. This covers C1 and C8. Document 09 section 9.7 and document 12 section 12.6.

**H2. String keys are integer codes.** Every string column with a dictionary has one dictionary per table, so a code means the same string in every segment. Group by, join and distinct on a string then run on 32-bit codes. rudb measured Q34 at 10 million rows going from 16.00 ms to 1.56 ms with global string codes (`../2140/storage-v3/09-global-string-codes.md`). This covers C2 and C3. Document 09 section 9.5.

**H3. Top-k by count is certified by a synopsis.** Each segment stores, for each column that is a candidate group key, the most frequent values with exact counts and an exact upper bound on the count of every value not in the list. A query that asks for the 10 most frequent keys merges the lists and the bounds to find the candidate keys. The bounds prove that no other key can enter the top 10. The query then reads the rows of the candidate keys only, and computes the groups, the counts and the order when it runs. The synopsis narrows the rows that the query reads. It never supplies the grouped output, because `../2140/storage-v3/34-no-query-answer-blocks.md` found that a stored top-k answer crosses the line of section 2.4.5. `../2140/storage-v3/11-certified-frequency-synopses.md` is the design. This covers most of C3. Document 12 section 12.8.

**H4. Substring search is pruned by gram summaries.** Each vector of a string column stores a small set summary of the 3-grams present, and each dictionary stores a gram index over its distinct values. `LIKE '%google%'` first finds the codes whose strings contain the pattern and then tests codes, not strings. This covers C5. Document 12 section 12.9.

**H5. Sort order and projections are chosen at load, from the data.** The load path chooses a sort order for each table and up to two sorted projections from the data statistics, without the queries. A range filter on the leading key then reads a few vectors. rudb measured Q9 at 4.6 ms against DuckDB's 93 ms with a sorted projection (`../2140/storage-v3/35-secondary-sorted-projections.md`). This covers C6 and C9. Document 10 section 10.7.

**H6. A function of one string column runs once for each distinct value.** `length(URL)` and `regexp_replace(Referer, ...)` are deterministic functions of one coded column. The executor evaluates them once for each code in the dictionary and keeps a code-to-result table. The cost is then the number of distinct values plus one table lookup for each row, not one regular expression for each row. This covers C7. Document 14 section 14.6.

These are the same levers that `../2140/02-the-goal.md` section 2.5 named for rudb: a heavy-hitter top-k, a global dictionary and the specialization of functions on strings. rudb's analysis found that if one of the three fails, the result falls to 4x to 6x of DuckDB. Against ClickHouse, which is 1.5x faster than DuckDB, a failure of one lever gives about 3x to 4x. We publish that number if it is the number. We do not move the target.

### 2.4.5 The honesty rule for summaries

A summary that answers a query must be a property of the data, not of the query. These rules apply to every structure that H1 to H6 depend on:

1. The structure is built by a rule that looks at the data and the schema. The rule never looks at the queries. The same rule runs for every table in every workload.
2. Its build time counts in the load time that we report.
3. Its bytes count in the data size that we report.
4. If you delete it, every answer stays the same, and only the time changes. The test suite in document 21 section 21.8 deletes every summary and runs every suite again.
5. It is maintained under `INSERT`, `UPDATE` and `DELETE`, with the same transactional guarantees as the data. A summary that is correct only for a table that never changes is not allowed.

This is the rule of `../2140/storage-v3/34-no-query-answer-blocks.md`, which stores only reusable metadata and never a query answer. The ClickBench rules for the untuned category must also accept these structures. Document 24 keeps that question open until the maintainers answer it, and we do not submit a result before they do.

### 2.4.6 The evidence so far

rudb is the nearest measured system to this design. At 10 million rows on an Apple M4 with 6 threads, rudb 0.8.5 ran the 43 queries on native files in 2.026 s, and DuckDB took 9.300 s, a ratio of 4.59x. Peak resident memory was 287 MiB against 1,144 MiB (`rudb-bench/reports/2026-09-28/main-2b6efff5-macos-clickbench.md`). A later run on the same day gave 5.51x. There is no rudb number on the full 100 million rows. The 2026-09-24 report says the full table did not fit on the test server.

If rudb's ratio of 5.5x to DuckDB held at 100 million rows, the hot sum would be 26.25 / 5.5 = 4.77 s. That is 2.7x away from 1.753 s. A ratio measured at 10 million rows does not predict the ratio at 100 million rows, so this is an estimate of the size of the gap, not a forecast.

The Bespoke OLAP ablation (Wehrstein, Eckmann, Jasny and Binnig, VLDB 2026) is the reason to expect the gap to close in storage and not in code. A flat columnar layout with specialized code gave 1.26x on TPC-H and 0.57x on CEB. A specialized layout gave 12.35x and 51.40x. The 2.7x that remains is in H3, H4 and H6, which are layout and summary work.

### 2.4.7 The cold run

ClickBench also reports a cold sum: the first run of each query after the operating system page cache is dropped. ClickHouse's cold sum is 113.72 s, and one tenth of it is 11.37 s. The `c6a.4xlarge` in ClickBench reads from a 500 GB gp2 EBS volume. AWS documents a maximum throughput of 250 MiB/s for a gp2 volume. At that rate 11.37 s reads at most 2.78 GiB, which is 66 MiB per query on average.

So the cold target is a limit on the bytes that a query reads, not on CPU. A query can read only the summaries and the vectors that the summaries select. The file layout of document 08 puts the summaries of a table in contiguous extents so that one sequential read loads them. The cold target is a gate at M8, together with the hot target.

### 2.4.8 The load

ClickBench reports the load time. ClickHouse loads in 177 s, DuckDB in 126 s and PostgreSQL in 1,004 s. One tenth of DuckDB is 12.6 s. DuckDB loads from `hits.parquet`, which is 14,779,976,446 bytes. If that file is not in the page cache, reading it at 250 MiB/s takes 56 s. So 12.6 s is below the physical floor of the machine for a load from a file on its disk, and a 10x load claim against DuckDB cannot hold on `c6a.4xlarge`.

The load target is therefore this. The load time is at most 1.25x the time to read the source file on the same machine, measured on the same run with `cat` to `/dev/null`. If the source is in the page cache when the load starts, the floor is CPU, and the target is 10x PostgreSQL and 2x DuckDB. Document 20 section 20.3 says how the run states which case it measured. The load includes every summary of section 2.4.5.

## 2.5 Axis 2: analytic speed on TPC-H

The target is 10x DuckDB on the TPC-H hot total at SF100, on the same machine, with the 22 queries run three times and the minimum taken for each. The DuckDB version is the newest release on the day of the run. DuckDB 2.0 is planned for 21 October 2026, so the first baseline is 2.0.

The evidence is weaker than on ClickBench. rudb measured 2.35x at SF1 hot and 1.60x at SF10 hot, and 0.75x at SF10 cold (`rudb-bench/reports/2026-09-26/compiler-baseline-tpch.md`). The `../2140/tenx/` work measured rudb at 1.85x behind DuckDB in instructions retired at SF1 on one thread, which leaves 18.5x to reach 10x by that metric. TPC-H has fewer repeated string keys than ClickBench and more joins, so H2 to H6 help less, and the levers are the clustered layout, the link graph of document 13 and exact semi-join reduction. On JOB, rudb measured 10.73x against DuckDB at the g2 gate (`../2140/bench/job/17-what-the-runs-taught.md` section 17.72), with the link graph and semi-join reduction. That result is the reason we keep 10x as the TPC-H target. It is a gate at M8 and it is the axis most likely to miss.

## 2.6 Axis 3: resources

### 2.6.1 What a resource is

ClickBench does not publish memory or CPU time. So we measure them for rupg and for every baseline on the same run. Each system runs in its own cgroup v2. We read three numbers.

**Peak memory.** `memory.peak` of the cgroup over the run of a suite. This counts process memory and the page cache that the cgroup charged. It counts the page cache because an engine that keeps its hot data in the page cache uses that memory as much as an engine that keeps it in its own buffers. A number for process memory alone, the proportional set size summed over the processes, is published next to it.

**CPU time.** `usage_usec` from `cpu.stat` of the cgroup, at the end of the suite minus at the start. This counts user and system time of every thread.

**Disk.** The bytes of the database files after the load and a checkpoint, as `du --apparent-size` gives them, plus any log or sidecar file that the system keeps.

### 2.6.2 Memory and CPU

The target is 10x on peak memory and on CPU time against each baseline, on the hot run of each suite. A hot run that reads summaries and selected vectors reads a small part of the file, so the memory target and the hot target support each other. At 10 million rows rudb measured 287 MiB against DuckDB's 1,144 MiB, which is 4.0x, with a design that did not have H3, H4 and H6. The memory target is a gate at M8.

The memory target has a second form for the server under many connections. PostgreSQL runs one process for each connection, and each backend has private memory for its catalog caches and its work memory. rupg runs sessions as tasks, and an idle session costs at most 64 KiB (document 06 section 6.4). At 1,000 idle connections the target is 10x less memory than PostgreSQL 19 with the same `shared_buffers` setting for the shared part.

### 2.6.3 Disk

Against PostgreSQL the target is 10x. PostgreSQL stores `hits` in 106,489,690,554 bytes, and one tenth is 10.65 GB. rudb has stored it in 9.65 GB (`../2140/03-baselines.md` section 3.5.1), so this target is met by a measured design.

Against ClickHouse and DuckDB the target cannot be 10x. ClickHouse stores `hits` in 9,451,237,939 bytes, and one tenth is 0.95 GB. rudb measured the three URL columns alone at 4.44 GB after front coding, FSST and dictionary work, and found 27,374,884 distinct values in `URL` (`../2140/02-the-goal.md` section 2.6.1). rudb also tested the cross-column rules that could remove content and found that only one held, and it saved 18 MB. Those three columns have more information than 0.95 GB can hold with the encodings that are known today. rupg will not reach 10x on disk against ClickHouse, and we do not claim it.

The disk target against ClickHouse and DuckDB is: smaller than every system on the board. The smallest today is elosdb at 7,668,470,543 bytes. The gate at M4 is smaller than ClickHouse (9.45 GB). The gate at M8 is smaller than 7.67 GB. The summaries of section 2.4.5 count in these bytes.

## 2.7 Axis 4: transactional speed

### 2.7.1 The floor

A statement that does no work still costs one round trip. On `gpc` (i9-13900K, WSL2), `SELECT 1` against PostgreSQL takes about 13 µs over a Unix socket and about 30 µs over TCP. A PostgreSQL point read takes 35 µs at one client, so PostgreSQL's own work in it is about 22 µs (`../2140/compat/postgres/15-performance.md` section 15.2). If rupg's work were zero, the latency at one client would fall from 35 µs to 13 µs, which is 2.7x. No server design can make it 10x.

So the 10x claim on transactional suites is made on server CPU per operation and on throughput per server core. The latency at one client is published, with this arithmetic next to it. It is not part of the claim.

### 2.7.2 TPC-C

The published PostgreSQL reference is 431,648 NOPM for PostgreSQL 18.1 and 444,364 NOPM for PostgreSQL 17.7, from HammerDB TPROC-C on a 48-core AMD EPYC 9454P with 128 GB, 1,000 warehouses, 40 virtual users and fsync on commit turned off (Small Datum, 15 February 2026). That is about 9,000 NOPM per core. We do not use it as a baseline. We measure PostgreSQL 19 on our machine with the same driver.

The target is 10x NOPM per server core against PostgreSQL 19, with stored procedures, which is how HammerDB drives PostgreSQL, on the same machine and with the same durability setting. With the reference number, that is about 90,000 NOPM per core. With plain statements, where each New-Order makes 5 + 2n round trips, the target is 3x, because of the floor of section 2.7.1. LeanStore reaches more than 1 million TPC-C transactions per second on one machine (Haas and Leis, PVLDB 16, 2023), so the target is inside what an engine of this design has shown. rudb has no measured TPC-C number (`../2140/bench/tpc-c/`), so rupg's TPC-C work starts from zero.

### 2.7.3 YCSB

The target is 10x server CPU per operation against PostgreSQL 19 for workloads A, B, C and F, with a pipeline depth of 64, and 3x without pipelining. These are the targets of `../2140/compat/postgres/15-performance.md` sections 15.7 and 15.8. PostgreSQL 18.6 on `gpc` gave 28,207 reads per second at one client and 227,753 at 16 clients (`../2140/bench/ycsb/06-the-baselines.md`).

### 2.7.4 Durability is the same on both sides

A published row compares rupg and PostgreSQL at the same durability. If PostgreSQL has `synchronous_commit = on`, rupg makes the commit durable before it answers. If PostgreSQL has `synchronous_commit = off`, rupg uses its asynchronous commit with the same window. A row with different durability on the two sides is not published.

## 2.8 Axis 5: scale out

The cluster of document 18 runs TPC-C with the warehouses spread over N nodes. The target is throughput of at least 0.8 N times the throughput of one node, for N = 2, 4, 8 and 16, with the standard share of remote transactions in TPC-C. The isolation level is snapshot isolation by default. Serializable is published as a second row. The gate runs with a replication factor of 1 on both sides, so it measures the cost of distribution and not the cost of replication. A row at replication factor 3 is published beside it (document 18). The reference is Mako (Shen, Cui, Sen, Angel and Mu, OSDI 2025), which reported 3.66 million TPC-C transactions per second on 10 shards. We do not compare with Mako's number, because it is not our machine and Mako is not PostgreSQL compatible.

For analytics, the target is that ClickBench on N nodes of `c6a.4xlarge` runs at least 0.7 N times as fast as on one node, for N up to 8. ClickBench has one table and no joins, so the scale-out cost is the exchange of partial aggregates. Document 18 section 18.8 gives the arithmetic.

Scale out is a gate at M11. Nothing before M10 depends on it, but the file format at M1 must already carry the shard number and the hybrid logical clock timestamps, and document 23 checks this at M1.

## 2.9 What we do not claim

**10x on single-client latency for point statements.** Section 2.7.1 gives the reason.

**10x on bytes on disk against ClickHouse and DuckDB.** Section 2.6.3 gives the reason.

**10x on load time against DuckDB from a cold source file.** Section 2.4.8 gives the reason.

**Binary compatibility with PostgreSQL C extensions.** Document 16 gives the reason.

**Byte-identical `EXPLAIN` text.** The plan that rupg chooses is a different plan, on a different storage layer. The node names and the JSON structure match PostgreSQL's vocabulary, and document 15 section 15.9 says how. The regression tests that compare plan text are counted in their own denominator.

**A number on hardware we did not run.** Every ratio in a rupg report has both sides measured by us on one machine.

## 2.10 When a gate fails

A gate either passes or fails on a measured number. When it fails, we publish the number, the gap and the cause that we found, in the milestone report. We do not change the target in the same change that reports the miss. To change a target, you must write the measurement that shows the target is wrong in document 24 first, as rudb did for its disk target in `../2140/02-the-goal.md` section 2.6.1.

| Gate | Axis | Suite and machine | Number | Milestone |
|---|---|---|---|---|
| G1 | compatibility | L1 and L2 suites | 100 percent of 7 clients connect and show the schema (psql, pgAdmin 4, DBeaver, psycopg 3, node-postgres, pgx and asyncpg; document 23) | M2 |
| G2 | analytic | ClickBench, `c6a.4xlarge` | hot sum below ClickHouse (17.53 s, remeasured) | M4 |
| G3 | resource | ClickBench, `c6a.4xlarge` | disk below 9.45 GB | M4 |
| G4 | transactional | YCSB A, B, C, F | 3x server CPU per operation without pipelining | M3 |
| G5 | transactional | TPC-C, statements | 3x NOPM per core | M5 |
| G6 | transactional | TPC-C, procedures | 10x NOPM per core | M7 |
| G7 | analytic | ClickBench, `c6a.4xlarge` | hot sum at most 1.753 s, cold sum at most 11.37 s | M8 |
| G8 | resource | ClickBench, `c6a.4xlarge` | 10x peak memory and CPU time against each baseline, disk below 7.67 GB | M8 |
| G9 | analytic | TPC-H SF100 | 10x DuckDB hot total | M8 |
| G10 | analytic | ClickBench, `c6a.metal` | hot sum at most 0.440 s | M8 |
| G11 | scale out | TPC-C, 2 to 16 nodes | at least 0.8 N | M11 |
| G12 | compatibility | L1 to L5, PostgreSQL 19 and shims 14 to 18 | 100 percent | M12 |
