# 20. Benchmarks

Written 7 October 2026. ClickBench is pinned to `github.com/ClickHouse/ClickBench` at `e6bda4e` (6 October 2026). PostgreSQL is pinned to `REL_19_STABLE` at `7d3d2db7` (19beta4). rudb is pinned to `cd9f9676`.

This document says how every performance number in a rupg report is produced: the machines, the suites, the transports, the rules for a fair run, and what each gate of document 02 section 2.10 reads. Document 02 owns the targets. Document 03 owns the published references. Document 19 owns the resource model. This document owns the procedure.

## 20.1 The rules

**Both sides are ours.** Every ratio in a rupg report has both numbers measured by us, on one machine, on the same day, with the same harness. A published number is a reference, and it never appears as one side of a ratio (document 00).

**Same transport.** rupg is measured through the transport that the baseline uses. When the baseline is a server that a client reaches over a socket, rupg runs as a server and the same kind of client reaches it over the same kind of socket. When the baseline runs in process, rupg runs in process through its library. An in-process number is never compared with a number measured over a socket (document 00).

| Baseline | How the baseline runs | How rupg runs for that comparison |
|---|---|---|
| PostgreSQL 19 | server, `psql` or `libpq` over a Unix socket | `rupg-server`, the same client over a Unix socket |
| ClickHouse | server, `clickhouse-client` over TCP on localhost | `rupg-server`, `psql` over TCP on localhost |
| DuckDB | in process, the `duckdb` command line program | in process, the `rupg` command line program of `rupg-cli` in embedded mode |
| Umbra or CedarDB | server, `psql` over the socket that its scripts use | `rupg-server`, the same |
| SQLite | in process | in process |

**Same durability.** A transactional row compares the two systems at the same durability (document 02 section 2.7.4). Analytic runs load with the default durability of each system, and the report states it.

**Versions are exact.** Each result records the exact version string of each system, the commit of the harness, the commit of rupg, the configuration file, the kernel version, the machine type and the date. A version that the system does not report is recorded from the package that was installed.

**Misses are published.** A run that misses a gate is published with the same detail as a run that passes, together with the gap and the cause that we found (document 02 section 2.10).

**No tuning for the benchmark in the gate row.** The gate row uses rupg's default configuration, with only the settings that every row of the suite sets: the memory limit when the baseline has one, `max_parallel_workers_per_gather` when the suite asks for all cores, and the log ring size for the disk number (document 19 section 19.10.1). Each such setting is printed in the report. A row with more settings is labeled tuned and does not decide a gate.

## 20.2 The machines

| Name | Machine | Use | Gates |
|---|---|---|---|
| `4xl` | AWS `c6a.4xlarge`: 16 vCPU AMD EPYC 7R13, 32 GiB, 500 GB gp2 EBS | ClickBench, as on the published board | G2, G3, G7, G8 |
| `metal` | AWS `c6a.metal`: 192 vCPU, 384 GiB, gp2 EBS | ClickBench | G10 |
| `tpch` | `c6a.4xlarge` for SF1 and SF10, and a machine with at least 64 vCPU, 256 GiB and local NVMe for SF100, chosen at M0 | TPC-H | G9 |
| `oltp` | a server with at least 32 cores and local NVMe with power loss protection, chosen at M0 | TPC-C, YCSB, connections | G4, G5, G6 |
| `cluster` | 16 machines of the `oltp` type in one placement group, for TPC-C, and 8 `c6a.4xlarge` for ClickBench | scale out | G11 |
| `gpc` | i9-13900K under WSL2 | daily development runs, never a gate | none |
| `m4` | Apple M4 laptop, 6 threads used | library and WebAssembly runs, comparison with rudb | none |

**Why `4xl` and `metal`.** They are the ClickBench machines. Our numbers can then be read next to the published board, and the rows of document 03 are on the same hardware.

**Why `oltp` has fast sync.** On `gpc` the `fdatasync` p50 is 2,247 µs, which caps any engine at about 385,000 NOPM with 32 terminals (document 03 section 3.12.2). A TPC-C run on such a device measures the device. The `oltp` machine must have an `fdatasync` p50 below 100 µs, measured at M0 with `fio` (`--rw=write --bs=4k --fdatasync=1 --iodepth=1`). This limit is a requirement for the choice of machine, not a measurement.

**Cores for the server and cores for the client.** On `oltp`, the database server runs in a cgroup with a `cpuset` of 16 cores, and the driver runs on the other cores. NOPM per core is NOPM divided by 16. The same `cpuset` is used for PostgreSQL and for rupg. On `4xl` and `metal`, the client and the server share the machine, as the ClickBench scripts do.

**Instance noise.** Two instances of the same type can differ. A gate on `4xl` or `metal` passes only if it passes on two separate instances, each started for the run. The report gives both numbers. The gate number is the worse of the two.

## 20.3 The load cases

The load target of document 02 section 2.4.8 has two cases. The run must say which case it measured, and it must prove it.

**Case A: the source is not in the page cache.** This is the case of the published ClickBench load times, because the scripts download the source file and then load it, and on a 32 GiB machine most of the file is no longer in memory. The target is that the load takes at most 1.25 times the time to read the source file once on the same machine.

**Case B: the source is in the page cache.** The target is at most one tenth of PostgreSQL's load time and at most one half of DuckDB's, each measured by us in the same case on the same machine.

### 20.3.1 How the run sets the case

For case A, the harness runs, in this order:

1. `sync`, then `echo 3 > /proc/sys/vm/drop_caches`.
2. `fincore` on the source file, which must report 0 resident pages. The output goes into the result.
3. The read time: `cat <source> > /dev/null`, timed. This is the reference time `R`.
4. `sync` and `drop_caches` again, and `fincore` again, which must report 0.
5. The load, timed. This is `L`.
6. The result records `L`, `R` and `L / R`. The case A target is `L / R <= 1.25`.

For case B, the harness runs `cat <source> > /dev/null` twice, then `fincore` on the source file, which must report all pages resident, then the load. If fewer than 99 percent of the pages are resident, the run is case A and is recorded as case A. On `4xl`, the 14.8 GB `hits.parquet` fits in the page cache. A larger source file may not fit, and then case B cannot be measured on that machine. The report says so.

The baselines are loaded in the same case as rupg, by the same steps, so case B for DuckDB means DuckDB loads from a source file that is in the page cache.

### 20.3.2 What the load includes

The load time ends when all of the following are true:

1. The statement or program that loads the data has returned, and the transaction is committed and durable.
2. Every row of the table is in its final layout: the mover of document 10 has moved the rows from the hot store into column segments, if the mover would move them later anyway.
3. Every summary that the honesty rule of document 02 section 2.4.5 allows the queries to use is built: segment summaries, global dictionaries, synopses, gram summaries and sorted projections.
4. A checkpoint has completed.

The harness checks these with the function `rupg_load_complete(regclass)`, which returns true when 2 and 3 are true for the table, and with `CHECKPOINT`. The harness polls the function every 100 ms after the load statement returns, and the load time includes the wait. A load that returns early and leaves the work to the background is measured to the end of the work.

**Why the rule is strict.** If the load time stopped when `COPY` returned, the summaries could be built in the background during the first queries, and the cold run would be slower than the published cold time, or the load time would hide work. ClickBench measures the load as the time of its load commands. rupg's ClickBench script calls `rupg_load_complete` in its load commands, so the published time includes everything.

### 20.3.3 The source file

PostgreSQL's ClickBench script loads `hits.tsv` with `COPY`. DuckDB's script loads `hits.parquet`. These files have different sizes, so `R` differs, and the load times of the two baselines are not on the same source.

rupg loads `hits.tsv` with `COPY FROM`, as PostgreSQL does, from M4. The Parquet reader of the importer of document 17 arrives at M9, and from then rupg also loads `hits.parquet`. Each load row names its source file, its size in bytes and its `R`. The case A ratio `L / R` is compared across sources. The case B ratios are compared only between systems that loaded the same source file. Which source the gate row uses before M9 is a question for document 24.

### 20.3.4 Resources during the load

The load is its own suite for document 19 section 19.2. The harness resets `memory.peak` before step 5 and reads it after the checkpoint. It records `usage_usec` and the `rbytes` and `wbytes` of `io.stat` over the same interval. The source file is read with buffered I/O and the pages are released after they are read (document 19 section 19.5), so the page cache charge of the load must stay small. The report gives the peak memory of the load for each system. rudb's streaming load used 33.6 MiB of resident memory against DuckDB's 1,953 MiB at 10 million rows (document 03 section 3.10), so a load peak far below the baselines is possible.

## 20.4 The resource measurements

Each system runs in its own cgroup v2 on the machine. The harness creates the cgroup, starts the system in it, and reads the files below. Document 19 section 19.2 gives the reasons.

| Number | File | When |
|---|---|---|
| Peak memory | `memory.peak` | reset by a write at the start of the suite, read at the end. On a kernel older than 6.12, a new cgroup for each suite. |
| Page cache part | `file` in `memory.stat` | sampled every 100 ms, the maximum is reported |
| Process memory | `Pss` in `/proc/<pid>/smaps_rollup`, summed over the processes of the cgroup | sampled every 100 ms, the maximum is reported |
| CPU time | `usage_usec` in `cpu.stat` | at the start and the end of the suite |
| Device bytes | `rbytes`, `wbytes` in `io.stat` | at the start and the end of the suite, and around each cold query |
| Disk at rest | `du --apparent-size --bytes` over the data directory or file, after a checkpoint | after the load |

**The suites for one ClickBench run.** A ClickBench run is three suites for resource purposes: the load, the cold runs and the hot runs. The cold runs and the hot runs are interleaved by the ClickBench scripts, three runs for each query. The harness measures the first run of each query as a cold suite and the second and third runs as a hot suite, by reading the counters around each run. `memory.peak` cannot be split this way inside one interval, so the harness resets it before the first run of each query and reads it after the third run. The ClickBench peak is the maximum over the 43 queries.

**Drop the caches between the load and the queries.** The harness runs `sync` and `drop_caches` after the load, so the page cache from the load does not count in the query peak.

**True cold runs.** The ClickBench rules at the pin ask for a true cold run: before the first run of each query, the operating system page cache is dropped and the database is restarted. rupg's ClickBench script does both. The restart time is not in the query time, because the ClickBench scripts time each query from the client. But the first query after a restart must read the file header, the catalog and the summaries that it needs, and that time is in the cold time.

**The idle base.** Before each suite, the harness measures an idle server with the loaded file for 10 s and records its `memory.peak` and `usage_usec`. These are published next to the suite numbers. They are not subtracted.

## 20.5 ClickBench

### 20.5.1 The run

rupg has a directory in our fork of the ClickBench repository with the same files as the PostgreSQL directory: `benchmark.sh`, `install`, `start`, `stop`, `check`, `create.sql`, `load`, `query`, `queries.sql`, `data-size` and `template.json`. `create.sql` and `queries.sql` are the PostgreSQL files without change, because rupg must accept the PostgreSQL schema and queries as they are. The `load` script runs the same `\copy hits FROM 'hits.tsv' with freeze` in one transaction as the PostgreSQL script, and then waits for `rupg_load_complete`. The `query` script is the PostgreSQL one with the connection changed. It starts a new `psql` for each run of each query and takes the time from the `\timing` line, so each run is a new session, and the time is measured at the client. The scripts use the shared driver `lib/benchmark-common.sh` of the pin, including its concurrent test with 10 connections for 600 s.

The result file has the ClickBench format, so it can be submitted. The harness adds a second file with the fields that ClickBench does not have: the resource numbers of section 20.4, the bytes read for each cold query, the `rupg_stat_file` breakdown of document 19 section 19.11, the versions and the configuration.

### 20.5.2 The rows of one ClickBench report

| Row | Configuration | Purpose |
|---|---|---|
| rupg | defaults, server, `psql` | the gate row against PostgreSQL, ClickHouse, Umbra and CedarDB |
| rupg embedded | defaults, `rupg` command line program in process | the gate row against DuckDB |
| rupg without summaries | `rupg.use_summaries = off`, and a file loaded without optional summaries (document 12) | shows what H1 to H6 give, and proves the answers do not depend on them |
| PostgreSQL 19 | ClickBench scripts | baseline |
| PostgreSQL 19 tuned | the settings of document 03 section 3.15, labeled tuned | reference |
| ClickHouse | ClickBench scripts, newest release | baseline |
| DuckDB | ClickBench scripts, newest release | baseline |
| Umbra or CedarDB | ClickBench scripts, if the license allows | baseline |

**The answers are checked.** ClickBench does not check results. The harness does. For each query, the harness compares rupg's result with PostgreSQL 19's result as a multiset of rows, after it sorts both by all columns for queries with `LIMIT` and ties. Floating point values are compared with a relative tolerance of 1e-9. A query with a wrong answer makes the run fail, whatever its time. The PostgreSQL answers are computed once on `metal` and stored with the harness.

### 20.5.3 What is reported for each query

For each of the 43 queries, the report gives: the three times, the cold time, the bytes read in the cold run, the peak memory, the CPU time of the hot runs, the class of document 02 section 2.4.3, the class budget, the time of each baseline, and the "Best" time of document 03 section 3.4. A query that is over its class budget is marked. For each class, the report gives the sum and the budget. This is how the gap of document 03 section 3.6 is tracked from M4 to M8.

### 20.5.4 Lever runs

Each of H1 to H6 is measured on its own, so a regression in one lever is visible even when the total improves.

| Lever | Queries | Measurement |
|---|---|---|
| H1 | C1, C8 | time and pages read with summaries on and off |
| H2 | C2, C3 | time with global codes and with codes for each segment |
| H3 | C3 | time, and the share of queries certified without a scan |
| H4 | C5 | time, and the codes tested against the strings tested |
| H5 | C6, C9 | time, and the vectors read |
| H6 | C7 | time, and the distinct values evaluated against the rows |

The lever runs use the same file and the same machine as the main run. The setting that turns one lever off is a test setting of document 12 or 14. The lever runs are not submitted to ClickBench.

## 20.6 The fixed cost of a query

The metal target is 0.440 s for 43 queries, or 10.2 ms each on average (document 02 section 2.4.1). On `metal` the fastest engines gain less than 2x from 12x the cores (document 03 section 3.3). So the fixed cost of a query is a large part of the target. The harness measures it with these probes on `4xl` and `metal`:

| Probe | What it measures |
|---|---|
| `SELECT 1`, 10,000 times, one connection | the round trip and the protocol |
| `SELECT count(*) FROM hits WHERE false`, 10,000 times | parse, analyze, plan and start, with no data |
| `EXPLAIN (SUMMARY) <query>` for each of the 43 queries | planning time |
| each of the 43 queries with `rupg.use_summaries` on and a `LIMIT 0` wrapper | the cost to start the pipelines |
| a query that fans out to all workers and returns one row | the cost to wake every worker and gather the result |

The report gives the median and the p99 of each probe. Because the ClickBench `query` script opens a new session for each run, the cost of a new session to reach its first plan is in every ClickBench time. rupg's catalog cache and plan cache are shared by all sessions (document 19 section 19.7), so a new session does not load the catalog again. The probes therefore run once on a new connection for each probe as well as on one long connection. The budget is 1 ms for the sum of the fixed costs of one ClickBench query on `metal`, which is about 10 percent of the average per query. This is a budget for M8, owned by documents 06 and 14.

## 20.7 TPC-H

**The data.** `dbgen` from the TPC-H tools, version 3.0.1, at SF1, SF10 and SF100. The same generated files are used for every system, loaded with each system's bulk loader from the `.tbl` files. The schema has the primary keys and foreign keys of the specification, because rupg uses them for links (document 13) and the other systems may use them.

**The queries.** The 22 queries from `qgen` with the default substitution parameters of the specification (`qgen -d`), so every system runs the same text. The PostgreSQL text is used for rupg and PostgreSQL. DuckDB uses the same text, which it accepts.

**The run.** Each query runs three times in a row, as in ClickBench. The hot total is the sum of the minimum of the second and third runs. The cold total is the sum of the first runs after a restart and a drop of the page cache. The answers are checked against the answer set of the specification at SF1 and against PostgreSQL 19 at SF10 and SF100.

**The transport.** The G9 gate compares rupg embedded with DuckDB in process. rupg as a server is compared with PostgreSQL 19 in a second row.

**The machine.** `tpch` (section 20.2). SF100 needs about 100 GB of raw data and more memory than `4xl` has, so the SF100 gate row runs on the larger machine and the same machine runs DuckDB.

**Instructions retired.** The harness records instructions retired with `perf stat -e instructions` for SF1 on one thread for rupg and DuckDB, as the `../2140/tenx/` work did. This shows whether a gain is in less work or in more parallel work (document 03 section 3.11).

## 20.8 TPC-C

**The driver.** HammerDB TPROC-C, the version current at M0, recorded in the result. HammerDB drives PostgreSQL with stored procedures by default. rupg runs the same procedures, which document 16 must support at M7. For the statement row, HammerDB's option to use client-side statements is used, so New-Order makes 5 + 2n round trips (document 02 section 2.7.2).

**The sizes.** Two warehouse counts on `oltp`: one whose data fits in the memory budget, and one 4 times larger. The Small Datum runs showed PostgreSQL losing 47 percent of its throughput from 1,000 to 4,000 warehouses (document 03 section 3.12.1). The exact counts are chosen at M0 from the memory of `oltp`.

**The run.** A ramp of 5 minutes, then a measured interval of 20 minutes. The virtual users are chosen at M0 for the peak of PostgreSQL 19 on `oltp`, and rupg is also run at that count and at the count where rupg peaks. The report gives both.

**The numbers.** NOPM, NOPM per core (section 20.2), the p50, p95 and p99 of each transaction type, `usage_usec`, `memory.peak`, the device write bytes and the write amplification of document 19 section 19.10.2. The sync bound, `0.45 x 60 x T / s` with `T` terminals and `s` the `fdatasync` p50 of the device (document 03 section 3.12.2), is printed next to each durable result.

**Durability.** Two rows: `synchronous_commit = on` for both systems, which decides G5 and G6, and `synchronous_commit = off` for both, which matches the published Small Datum setting.

**Correctness.** After each run, the harness runs the consistency conditions of the TPC-C specification, clause 3.3.2, on both systems. A run that fails one is not published as a result.

**The mixed run.** The interference target of document 14 section 14.11 is measured with TPC-C and the ClickBench queries at the same time on one rupg server: TPC-C on its warehouses with the `oltp` virtual users, and one connection that runs the 43 ClickBench queries in a loop on `hits` in the same file. The report gives NOPM and the ClickBench hot sum with each workload alone and with both together. A version of the CH-benCHmark with the analytic queries on the TPC-C tables is a second mixed run from M8.

## 20.9 YCSB

**The driver.** The YCSB driver of `../2140/bench/ycsb/04-the-driver.md`, ported to the PostgreSQL protocol through `libpq`. Workloads A, B, C and F with the zipfian constant 0.99 and the record count chosen at M0 to fit in the memory budget.

**The rows.** Each workload at 1 client and at 16 clients, without pipelining, and at 16 clients with the `libpq` pipeline mode at a depth of 64. The G4 gate reads the 16-client rows without pipelining. The 10x target of document 02 section 2.7.3 reads the pipelined rows.

**The numbers.** Throughput, latency percentiles, `usage_usec` divided by the operations, which is CPU per operation, and `memory.peak`. For updates, both durability rows as in section 20.8. Under a zipfian load, the update rate of the hottest key is reported, because with sync on it is bounded by the sync rate on PostgreSQL (document 03 section 3.13).

**In process.** rupg embedded is compared with SQLite on workload C at 1 client, as a sanity row. It is not a gate.

## 20.10 Connections

The connection target of document 19 section 19.7. The harness opens 100, 1,000 and 10,000 connections to rupg and to PostgreSQL 19 with the same `shared_buffers`, each connection runs `SELECT 1` once, and then all connections stay idle for 60 s. The report gives `memory.peak`, the sum of `Pss`, and `usage_usec` over the idle 60 s. PostgreSQL needs `max_connections` raised for this run, which the report states.

## 20.11 Scale out

**TPC-C.** The warehouses are spread over N nodes, for N = 1, 2, 4, 8 and 16, with the standard share of remote transactions. The number of warehouses grows with N, so each node has the same load. The report gives NOPM for each N and the ratio to N times the NOPM of one node. G11 passes when the ratio is at least 0.8 for every N. Snapshot isolation decides the gate and serializable is a second row (document 02 section 2.8).

**ClickBench.** `hits` sharded over N `c6a.4xlarge` nodes for N = 1, 2, 4 and 8, by a hash of `UserID`. The client connects to one node. The report gives the hot sum for each N and the ratio to the one-node time divided by N. The target is at least 0.7. Document 18 section 18.8 gives the arithmetic of the partial aggregates. There is no published baseline for this run that uses the same rules, so it is reported as rupg against itself.

## 20.12 The harness

The harness is a separate repository, `rupg-bench`, in the way that `rudb-bench` is separate from rudb. It does not depend on the rupg crates. It talks to every system through its client program or `libpq`. It contains:

1. The suite drivers of sections 20.5 to 20.11, or the scripts that call the external drivers.
2. The cgroup runner of section 20.4.
3. The answer sets: the PostgreSQL 19 answers for ClickBench and TPC-H, and the TPC-H answer set at SF1.
4. The report generator, which writes `reports/<date>/<commit>-<machine>-<suite>.md` and a JSON file for each run.
5. The machine scripts that start a `4xl`, `metal` or `oltp` instance, install the pinned versions, run a suite and stop the instance.

**Daily runs.** Every commit to the main branch of rupg runs ClickBench at 10 million rows, TPC-H at SF1 and YCSB C at 1 client on `gpc`, and the report compares with the previous day. A regression of more than 5 percent on any suite total opens an issue. These runs do not decide gates.

**Gate runs.** A gate run is started by hand at the end of a milestone. It runs the full suite on the gate machines, with the baselines, and its report is the milestone evidence.

## 20.13 What each gate reads

| Gate | Suite | Machine | Row | Pass rule |
|---|---|---|---|---|
| G1 | the client suites of document 05 | any | 7 clients | each client connects and shows the schema |
| G2 | ClickBench | `4xl`, two instances | rupg against ClickHouse | hot sum below ClickHouse's, measured in the same run |
| G3 | ClickBench | `4xl` | rupg | disk below 9.45 GB after load and checkpoint |
| G4 | YCSB A, B, C, F | `oltp` | 16 clients, no pipelining | 3x less server CPU per operation than PostgreSQL 19 on every workload |
| G5 | TPC-C, statements | `oltp` | sync on | 3x NOPM per core of PostgreSQL 19 |
| G6 | TPC-C, procedures | `oltp` | sync on | 10x NOPM per core of PostgreSQL 19 |
| G7 | ClickBench | `4xl`, two instances | rupg | hot sum at most 1.753 s and cold sum at most 11.37 s, with correct answers |
| G8 | ClickBench | `4xl` | rupg and rupg embedded | 10x lower peak memory and CPU time than each baseline, through its transport, and disk below 7.67 GB |
| G9 | TPC-H SF100 | `tpch` | rupg embedded | hot total at most one tenth of DuckDB's |
| G10 | ClickBench | `metal`, two instances | rupg | hot sum at most 0.440 s |
| G11 | TPC-C | `cluster` | snapshot isolation | at least 0.8 N for N up to 16 |
| G12 | the suites of document 05 | any | all | 100 percent |

**G2 and G7 use our ClickHouse number.** The 17.53 s of document 02 is the published number. The gate uses ClickHouse measured in the same run. If our ClickHouse number differs, the 1.753 s target of G7 does not move, because document 02 fixes it. The difference is reported.

**The summaries row must pass the correctness check.** G7 and G10 pass only if the "rupg without summaries" row of section 20.5.2 gives the same answers as the main row. That is the honesty rule of document 02 section 2.4.5, and document 21 section 21.8 runs it as a test as well.

## 20.14 What we publish

For each gate run, we publish the report, the result files, the configuration files, the harness commit and the instance types. For ClickBench, we submit the rupg result to the ClickBench repository only after the maintainers have said that the summaries of document 02 section 2.4.5 are allowed in the untuned category. Until then, the results are published in `rupg-bench` only. Document 24 holds that question.

A result that we later find wrong is corrected in a new report that names the old one. The old report stays.
