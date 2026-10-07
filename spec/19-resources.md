# 19. Resources

Written 7 October 2026. PostgreSQL behavior is from `REL_19_STABLE` at `7d3d2db7` (19beta4). ClickBench facts are from `github.com/ClickHouse/ClickBench` at `e6bda4e`. rudb facts are from `cd9f9676`.

Axis 3 of document 02 asks for 10x less memory and 10x less CPU than each baseline, and for less disk than every system on the ClickBench board. This document says how rupg uses memory, CPU, disk and network, which settings control each one, and how each one is measured. Document 20 runs the measurements. Document 02 section 2.6 owns the targets.

## 19.1 The principles

**One budget for memory.** A node has one number, `rupg.memory_limit`, and every consumer of memory takes its share from that number (document 04 section 4.6). There is no second pool that a user must size. A consumer that cannot get memory spills, waits or fails with a PostgreSQL error. The process never grows past the budget because of data.

**The engine owns its caching.** rupg reads its file with direct I/O where the platform allows it. The operating system page cache does not hold a second copy of the pages that the buffer pool holds. Section 19.5 gives the reasons and the exceptions.

**Idle costs almost nothing.** An idle session costs at most 64 KiB (document 06 section 6.4). An idle library handle costs less than 1 MiB and starts no thread. An idle server uses no CPU except a timer for each worker.

**Background work has a share, not a priority.** The mover, compaction, statistics and checksum scrubbing run in queues 3 and 4 of document 04 section 4.5. They are limited by a share of the CPU and a share of the I/O, so they cannot take the machine from foreground work, and they cannot starve when there is no foreground work.

**Every resource is visible.** Each consumer reports what it holds through a view. A user can find out where the memory is without a debugger.

## 19.2 What we count

The measurement rules are fixed in document 02 section 2.6.1. This section repeats them with the reasons, because the design follows from what is counted.

| Resource | Number | Source | Why this number |
|---|---|---|---|
| Memory | Peak memory of the cgroup | `memory.peak` in cgroup v2 | It counts process memory and the page cache that the cgroup charged. An engine that keeps its data in the page cache uses that memory. |
| Memory, second view | Proportional set size summed over the processes | `/proc/<pid>/smaps_rollup`, field `Pss`, sampled every 100 ms | It shows process memory alone. It is published next to `memory.peak` and does not decide a gate. |
| CPU | CPU time of the cgroup | `usage_usec` in `cpu.stat`, end minus start | It counts user and system time of every thread, including I/O threads and background work. |
| Disk at rest | Bytes of all files of the database | `du --apparent-size --bytes` after load and checkpoint | It counts the data, the summaries, the free space inside the file and any sidecar file. |
| Disk traffic | Bytes read and written by the cgroup | `rbytes` and `wbytes` in `io.stat` | It counts the I/O that the device sees, which the cold target and write amplification depend on. |
| Network | Bytes sent and received on the client socket | the harness, from the socket counters | It counts protocol overhead and result size. |

**Why the page cache counts.** If `memory.peak` left out the page cache, an engine could keep its whole data set in the page cache and report a small number. Several of the baselines use buffered I/O and rely on the page cache. Counting it is the only way to compare an engine that caches in its own buffers with one that caches in the kernel.

**How the peak is reset.** Since Linux 6.12, a write to `memory.peak` resets the peak for later reads through the same file descriptor. The harness of document 20 opens `memory.peak`, writes to it at the start of each suite, and reads it at the end. On an older kernel the harness creates a new cgroup for each suite. The load and the query runs are separate suites, so the page cache that the load charged does not count in the query peak. The harness drops the page cache between them (document 20 section 20.4).

## 19.3 The memory budget

`rupg.memory_limit` is the one budget. Its default is 25 percent of physical memory for the server and 256 MiB for the library (document 04 section 4.6). In a container, physical memory is the smaller of the machine memory and `memory.max` of the cgroup. The value can be set in the configuration file, on the command line, or by `ALTER SYSTEM`. It takes effect without a restart. When it goes down, consumers give memory back in the order of section 19.3.2.

### 19.3.1 The consumers

| Consumer | Minimum | Can grow to | What happens at the limit | Owner |
|---|---|---|---|---|
| Buffer pool | `shared_buffers` | everything that other consumers do not hold | evicts pages | document 08 section 8.8 |
| Operator memory of running queries | 0 | `work_mem` for each operator by default, more by grant | spills, or waits for a grant | document 14 |
| Undo buffers | 1 MiB for each worker | 25 percent of the budget | writes undo pages to the file | document 11 |
| Log rings | fixed by the file format | fixed | not a variable consumer | document 11 |
| Plan cache | 0 | 5 percent of the budget | evicts the least recently used plan | document 15 |
| Catalog cache | the built-in catalog, which is static | 5 percent of the budget | evicts entries of user objects | document 07 |
| Session state | 64 KiB for each idle session | 64 KiB plus the state of the running statement | refuses a new connection with SQLSTATE `53300` | document 06 |
| Dictionaries and summaries | 0 | part of the buffer pool | they are pages, so the buffer pool evicts them | documents 09 and 12 |
| Compiled code | 0 | 64 MiB | evicts the least recently used pipeline | document 14 |

The shares are budgets that M4 and M8 measure. They are not final.

**Dictionaries and summaries are pages.** They are stored in the file as pages and extents, and they are read through the buffer pool like any other page. They are not a separate cache. A summary that the buffer pool evicts is read again from the file when a query needs it. This keeps one eviction policy for all data and makes the cold run honest: after a restart, the summaries are not in memory.

### 19.3.2 The order of release

When the budget is short, consumers release memory in this order:

1. The buffer pool evicts clean pages that no query has touched in the last second.
2. The plan cache and the compiled code cache evict their least recently used entries.
3. The buffer pool writes and evicts dirty pages.
4. New operator reservations wait in a queue for at most `rupg.grant_timeout`, default 1 s, and then spill.
5. New connections are refused with SQLSTATE `53300` and the PostgreSQL message for too many connections.

A query that already holds a reservation keeps it until it finishes or spills. rupg never kills a running query to free memory. If a single operator needs more memory than the budget allows and cannot spill, the query fails with SQLSTATE `53200`, out of memory, which is the error PostgreSQL gives.

### 19.3.3 What the budget does not cover

The budget does not cover the code and static data of the binary, the stacks of the worker threads, or memory that the allocator holds but does not use. These are bounded and small. The harness measures them as the `memory.peak` of an idle server with an empty file. The target for that number is 32 MiB for the server and 8 MiB for the library, which is a budget for M2.

## 19.4 PostgreSQL memory settings in rupg

Clients and tools read and set the PostgreSQL memory settings, so each one must exist with its PostgreSQL name, unit and default. Each one maps to a part of the rupg design.

| Setting | PostgreSQL 19 default | Meaning in rupg |
|---|---|---|
| `shared_buffers` | 128MB | The minimum size of the buffer pool. The buffer pool can grow past it up to the budget. |
| `work_mem` | 4MB | The reservation that one operator takes before it spills, when the planner has no larger estimate. rupg raises the reservation up to 16 times `work_mem` when the estimate needs it and the budget allows it. |
| `hash_mem_multiplier` | 2.0 | Multiplies `work_mem` for hash operators, as in PostgreSQL. |
| `maintenance_work_mem` | 64MB | The reservation of `CREATE INDEX`, the mover, and `VACUUM`, which is compaction in rupg. |
| `autovacuum_work_mem` | -1 | The reservation of background compaction. -1 means `maintenance_work_mem`. |
| `temp_buffers` | 8MB | The buffer pool share of the temporary tables of one session. |
| `effective_cache_size` | 4GB | A planner input only. rupg sets its default to `rupg.memory_limit`. |
| `temp_file_limit` | -1 | The maximum bytes of spill extents that one session can hold in the file. -1 means no limit. |
| `max_connections` | 100 | The maximum number of sessions. In rupg a session costs 64 KiB when idle, so the default is 100 for compatibility, and a value of 10,000 is reasonable. |
| `huge_pages` | try | Whether the buffer pool reservation uses huge pages. |
| `logical_decoding_work_mem` | 64MB | The memory of a logical replication sender before it spills. |

`SHOW` returns the value in the unit and form of PostgreSQL. `pg_settings` shows the PostgreSQL default in `boot_val` and the rupg default in `reset_val` if they differ, as PostgreSQL does for a setting that was set in the configuration file.

**The new settings.** rupg adds these settings, with the prefix that document 05 reserves:

| Setting | Default | Meaning |
|---|---|---|
| `rupg.memory_limit` | 25% of memory, or 256MB in the library | The one budget. |
| `rupg.grant_timeout` | 1s | How long a reservation waits in the queue before it spills. |
| `rupg.background_cpu_share` | 0.25 | The share of worker time that background work can use when foreground work waits. |
| `rupg.background_io_share` | 0.25 | The share of device bandwidth that background work can use when foreground work waits. |
| `rupg.direct_io` | on | Whether the file is read and written with direct I/O. Section 19.5. |

## 19.5 The buffer pool and the page cache

**The server uses direct I/O.** On Linux the file is opened with `O_DIRECT`, and the buffer pool reads pages into its own memory through `io_uring`. On macOS the file is opened and then marked with `fcntl(F_NOCACHE)`. On Windows, which is a later platform, the file uses `FILE_FLAG_NO_BUFFERING`. The reasons are these:

1. A page that is in the buffer pool and in the page cache counts twice in `memory.peak`. With buffered I/O, a cold run that reads 2.78 GiB would charge 2.78 GiB of page cache and up to 2.78 GiB of buffer pool.
2. The engine knows which pages it will need. The vmcache design of document 08 section 8.8 decides eviction from access counts that the kernel does not see.
3. The out-of-place writes of document 08 section 8.6 need control of the write order, and direct I/O with explicit `fdatasync` gives it.
4. Lee, Ziegler and Leis (PVLDB 19, 2026) measured their out-of-place write gains with direct I/O. We want the same conditions.

**The exceptions.** Direct I/O needs aligned buffers and aligned offsets. rupg pages are 16 KiB and extents start on page boundaries, so all data I/O is aligned. When the file system does not support direct I/O, for example tmpfs, the open falls back to buffered I/O and logs one warning. The library sets `rupg.direct_io` to on as well, but an embedder can set it off, for example on a laptop where the page cache helps other programs that read the same file. In WebAssembly the file goes through the Origin Private File System and there is no page cache to control.

**Buffered I/O for import.** `COPY FROM` a file and the importer of document 17 read the source file with buffered I/O and `posix_fadvise(POSIX_FADV_SEQUENTIAL)`, and they call `posix_fadvise(POSIX_FADV_DONTNEED)` on each range after they read it. So a load does not leave the source file in the page cache. Document 20 section 20.3 depends on this.

**The size of the buffer pool.** The buffer pool reserves virtual address space for the whole file and maps a page only when it is read (document 08 section 8.8). Its memory is the pages that it holds, not the size of the reservation. It starts small, grows as pages are read, and shrinks when other consumers need memory. `shared_buffers` sets the size below which it does not shrink.

## 19.6 Operator memory and spill

**Reservations.** Before an operator builds a hash table, a sort buffer or an aggregate state, it reserves memory from the budget. The reservation is in units of 1 MiB. The planner gives each operator an estimate. The operator asks for the smaller of its estimate and the larger of `work_mem` and the grant limit. If the budget has room, the reservation is granted at once. If not, the request waits in a queue for at most `rupg.grant_timeout`, and then the operator starts with `work_mem` and spills when it needs more.

**Spill goes to the file.** There is no temporary file. A spilling operator writes runs or partitions to spill extents in the `.rupg` file, through the same I/O layer, with the same checksums. Spill extents are not in the log and are not reachable from a checkpoint. After a crash, recovery returns every spill extent to the free space map. At the end of each statement, the operator frees its spill extents at once. `temp_file_limit` limits the spill bytes of one session, and a session that passes it fails with SQLSTATE `53400`, as in PostgreSQL. `pg_stat_database.temp_files` and `temp_bytes` count spill extents, so monitoring tools see the same counters.

**The file must not grow because of spill.** A spill extent is taken from free space first. When the file must grow for spill, the growth is at the end of the file, and when the spill is freed, the checkpoint truncates trailing free extents. So a query that spills 10 GB leaves the file at its old size after the next checkpoint. The disk measurement of document 02 is taken after a checkpoint for this reason.

**Spill degrades smoothly.** This is the design of Kuiper, Groß, Boncz and Mühleisen (PVLDB 18, 2025). A hash aggregate partitions its input by the high bits of the hash and spills only the partitions that do not fit. A hash join spills the build side by partition. A sort writes runs and merges them. In each case the time grows with the bytes that spill, not as a step when the first byte spills. Document 14 owns the operators. Document 21 tests each one with a memory limit of 16 MiB on the TPC-H queries at SF1.

## 19.7 Sessions and connections

**An idle session is at most 64 KiB.** It holds the session settings that differ from the defaults, the prepared statement names and their plan references, the input and output buffers, and the protocol state. The buffers start at 8 KiB each and shrink back to 8 KiB after a large message. A prepared statement holds a reference to a shared plan, not a copy. Document 06 section 6.4 owns the layout.

**Caches are shared, not per session.** PostgreSQL keeps a catalog cache, a relation cache and a plan cache in each backend process. With 1,000 connections, each table definition is in memory 1,000 times. rupg has one catalog cache and one plan cache for the node, and sessions read them under MVCC. This is the main reason for the connection target of document 02 section 2.6.2.

**The connection target.** At 1,000 idle connections, rupg's `memory.peak` must be at most one tenth of PostgreSQL 19's with the same `shared_buffers`. This is a prediction of the shape of the result: 1,000 sessions at 64 KiB is 62.5 MiB, plus the base of section 19.3.3. PostgreSQL's private memory for each backend is not measured yet. M0 measures it (document 03 section 3.15).

**An active session.** While a statement runs, the session holds a stack for its future, the state of its operators and the reservations of section 19.6. A point statement on the point path of document 14 has no plan tree and no operator reservation. Its working memory is the session buffers and a fixed scratch area of 16 KiB. So a server with 10,000 connections that each run point statements does not need operator memory for them.

## 19.8 The library and WebAssembly

**The library default is 256 MiB.** A program that embeds rupg does not expect the database to take a quarter of the machine. The default can be set at open time through the Rust API, the C API or a connection string option (document 17).

**The library starts no threads.** The calling thread runs the statement, and a query uses more threads only from a pool that the program enables (document 04 section 4.5). So the CPU of the library is the CPU of the calling thread, unless the program asks for more.

**WebAssembly has a hard limit.** A 32-bit WebAssembly module can address at most 4 GiB, and browsers often allow less. The WebAssembly build sets the budget to the smaller of 256 MiB and half of the memory that the module can grow to. The buffer pool uses a hash table from page number to frame, because WebAssembly has no virtual memory reservation (document 08 section 8.8). Spill goes to the same file in the Origin Private File System.

**Memory64.** When the runtime supports 64-bit WebAssembly memory, the build can use a larger budget. Whether to ship a Memory64 build at M9 is a question for document 24.

## 19.9 CPU

### 19.9.1 Workers

The server runs one worker for each core it may use. On Linux that is the number of CPUs in the affinity mask, limited by the CPU quota of the cgroup: a quota of 4 CPUs gives 4 workers even on a 64-core machine. The setting `max_worker_processes` exists for compatibility. In rupg it limits the number of workers if it is lower than the number of cores. `max_parallel_workers_per_gather` limits how many workers one query can use for its morsels. The default is the PostgreSQL default of 2. For the ClickBench and TPC-H runs of document 20, it is set to the number of workers, and the run reports this as a configuration line.

**The default of 2 is an open question.** PostgreSQL's default is 2, and a client that reads it must see 2. But an analytic engine that uses 2 of 16 cores is 8x slower than it can be. The run must report every setting that differs from the default, and ClickBench rules allow settings in the configuration. Document 24 has the question of whether rupg's default should differ from the value it reports.

### 19.9.2 Background work

Background work is the mover, compaction, statistics, index builds started by `CREATE INDEX CONCURRENTLY`, and checksum scrubbing. It runs in queues 3 and 4 of document 04 section 4.5. A worker takes background work only when its queues 1 and 2 are empty, or when background work has used less than `rupg.background_cpu_share` of the worker's time in the last second. The second rule keeps the mover moving under a constant analytic load, so the hot store does not grow without limit. The I/O share works the same way on the bytes submitted to the device.

**The mover has a deadline.** If the hot store of a table grows past 4 segments' worth of rows, which is 1,048,576 rows, the mover share for that table rises to 0.5 until the hot store is below 2 segments. This is a budget for M4, and document 10 owns the mover.

### 19.9.3 CPU accounting

Each worker counts the CPU time it spends on each task, from the thread CPU clock, at each task switch. The counts are added to the session, to the statement, and to the background job. They appear in `rupg_stat_cpu` (section 19.11) by worker and by kind of work. The PostgreSQL views keep their meaning: `pg_stat_statements.total_exec_time` and the times of `EXPLAIN (ANALYZE)` are wall time, as in PostgreSQL, and rupg does not add CPU columns to them.

**Spinning is not allowed.** A worker with no work parks on its `io_uring` ring or its `kqueue` with a timeout of 10 ms. It does not spin. A spinning worker would make the CPU number of an idle server nonzero, and the CPU target counts every microsecond.

## 19.10 Disk

### 19.10.1 The bytes at rest

The file holds the data, the summaries, the indexes, the catalog, the log rings, the free space map and free extents. All of them count in the disk number. The design rules that keep the number small are:

1. **The log rings have a fixed size.** Each worker ring is 64 MiB by default, so 16 workers use 1 GiB. This is a budget. A checkpoint releases the part of a ring that recovery no longer needs, and the ring reuses it. The ring size is in the file header and is the same after a restart. Document 11 owns it.
2. **Free space is reused before the file grows.** The free space map of document 08 gives out the lowest free extent that fits.
3. **The file shrinks at checkpoint.** After a checkpoint, free extents at the end of the file are truncated.
4. **Compaction rewrites half-empty segments.** A segment with more than 25 percent deleted rows is a candidate. Compaction writes a new segment and frees the old one when no snapshot can see it.
5. **Summaries have a size budget.** `rupg_summary_size(regclass)` reports their bytes (document 12). They count in the disk number by the honesty rule of document 02 section 2.4.5.

**The disk number for ClickBench.** At M4, G3 asks for less than 9.45 GB. The log rings of the ClickBench machine with 16 workers are 1 GiB of that if they are at the default size. That is too much. So the ClickBench run must use a smaller ring, and the ring size is a configuration line of the run. A ring of 8 MiB for each worker is 128 MiB. Whether a ring that small is enough for the load is a question that M1 answers with a measurement.

### 19.10.2 The bytes in motion

**Write amplification.** Document 08 section 8.6 writes pages out of place. The harness measures the device write bytes from `io.stat` for each TPC-C and YCSB run, and divides by the bytes of logical change, which is the size of the log records of the run. The target is a ratio below 3 on TPC-C. This is a budget, and M5 measures it.

**Read bytes in the cold run.** The cold target of document 02 section 2.4.7 limits the cold run to 2.78 GiB of reads at 250 MiB/s. The harness records `rbytes` from `io.stat` for each cold query, and document 20 publishes the bytes read for each query. A query that reads more than 66 MiB is not a failure on its own, but the sum over the 43 queries is the cold budget.

## 19.11 How a user sees resources

The PostgreSQL views must keep their columns and meaning. rupg adds its numbers in new views and in new columns only where document 05 allows a new column.

| View or function | Content | New or PostgreSQL |
|---|---|---|
| `pg_backend_memory_contexts` | One row for each consumer of section 19.3.1 that the session holds, with the PostgreSQL column names | PostgreSQL |
| `pg_stat_io` | I/O counts by backend type, object and context | PostgreSQL |
| `pg_stat_database.temp_files`, `temp_bytes` | Spill extents | PostgreSQL |
| `rupg_stat_memory` | One row for each consumer of section 19.3.1 for the node: bytes held, bytes reserved, the minimum, the limit | new |
| `rupg_stat_cpu` | CPU microseconds by worker and by kind of work: session, query morsel, mover, compaction, statistics, scrub, idle | new |
| `rupg_stat_file` | File bytes by kind: hot pages, cold segments, summaries, indexes, catalog, log rings, spill, free | new |
| `rupg_summary_size(regclass)` | Bytes of summaries of one table | document 12 |
| `EXPLAIN (ANALYZE, MEMORY)` | Planner memory, as in PostgreSQL 17 and later | PostgreSQL |
| `EXPLAIN (ANALYZE, BUFFERS)` | Pages hit, read and written, as in PostgreSQL | PostgreSQL |

`pg_backend_memory_contexts` has a fixed set of rows in rupg, one for each consumer, named with the PostgreSQL context names where a matching one exists: `CacheMemoryContext` for the catalog cache, `CachedPlan` for plans, `ExecutorState` for operators. Tools that sum `total_bytes` get a correct total. The rows do not have the tree of contexts that PostgreSQL has, and the `parent` and `path` columns show one level. Document 05 counts this view in the denominator of catalog views with known differences.

**`rupg_stat_file` is the disk report.** The harness reads it after each load, and document 20 publishes it with the disk number. It is how a reader sees that, for example, the URL columns are 46 percent of the file.

## 19.12 Budgets for the gates

### 19.12.1 Memory on ClickBench

ClickBench runs each query three times in a row before it moves to the next query. So the hot runs need the working set of one query in memory, not the working set of all 43. The memory peak of a run is the largest working set of a single query, plus what the buffer pool keeps from earlier queries when there is room.

The cold target gives an average of 66 MiB of reads for each query (document 02 section 2.4.7). If the hot run of a query reads the same pages as its cold run, which is the intent of the design, then most queries need less than 100 MiB of pages. The exceptions are the queries that must read a whole dictionary or a whole column:

| Query | What it must read | Estimate |
|---|---|---|
| Q28 | the dictionary of `Referer`, to run the regular expression once for each distinct value (H6) | 26,590,624 strings. rudb's three URL columns take 4.44 GB in total. The `Referer` dictionary is a large part of one third of that, so the estimate is 1 GB to 1.5 GB. |
| Q27 | the lengths of the `URL` dictionary entries | about 27 million lengths at 4 bytes, 110 MB, if the dictionary stores lengths separately |
| Q32 | `WatchID` and `ClientIP` for all rows, unless H3 certifies the top 10 | 100 million rows at 12 bytes, 1.2 GB of input, and a hash table of about 100 million groups |
| Q4 | the distinct `UserID` values | about 17.6 million values, 141 MB as raw 64-bit keys |

These estimates are predictions from the data facts of document 03 section 3.8. They are not measurements.

**Q32 sets the peak.** A plain hash aggregate on Q32 holds about 100 million groups. With a 12-byte key, an 8-byte count, an 8-byte sum and a 16-byte average state, each group is about 44 bytes before the overhead of the table, so the table is 4.4 GB or more. That is more than the whole memory budget we expect to have. So Q32 must not be run as a plain hash aggregate. There are two ways: H3 certifies the top 10 from the segment synopses and reads only the candidate rows (document 12 section 12.8), or the aggregate is partitioned by hash and runs one partition at a time, which reads the input more than once. Document 14 must choose. Document 24 has the question.

**The ClickBench budget.** The design budget for `memory.peak` on the ClickBench hot run is 2 GiB, with `rupg.memory_limit` at its default of 25 percent of 32 GiB, which is 8 GiB. The buffer pool will not fill to 8 GiB, because the run does not read that much. This budget is a prediction that M0 must check: G8 asks for 10x less than each baseline, and if a baseline's peak is less than 20 GiB, 2 GiB is not enough. In that case the target for rupg is one tenth of the lowest baseline, and the run sets `rupg.memory_limit` to that value and reports it.

### 19.12.2 CPU on ClickBench

At the hot target of 1.753 s and 16 vCPUs, the most CPU that the hot runs can use is 2 x 1.753 s x 16 = 56 vCPU seconds for the two hot runs. The cold run and the load add to it. ClickBench does not publish CPU time, so the 10x target against each baseline has no number until M0. Document 20 records `usage_usec` for the cold run, the hot runs and the load as three numbers.

The CPU target and the hot target pull in the same direction only if the work is parallel. A query that takes 10 ms on 16 workers spends up to 160 ms of CPU. A query that takes 15 ms on 2 workers spends 30 ms. For short queries, the run with fewer workers can win on CPU and lose on time. Document 14 must give each query the number of workers that its estimated work justifies, with a minimum of about 1 ms of work for each morsel (document 04 section 4.5).

### 19.12.3 Memory and CPU on TPC-C and YCSB

The TPC-C target is 10x NOPM per core. The memory for TPC-C is the buffer pool for the warehouses that are hot, the undo buffers and the sessions. HammerDB uses one connection for each virtual user, so the sessions are a small number. The memory number for TPC-C is published, and it is not a gate. It must not be higher than PostgreSQL's at the same buffer pool size, which is a budget.

The YCSB CPU target is per operation: `usage_usec` divided by the operations of the run. On `gpc` PostgreSQL reads at about 14,000 per core at 16 clients (document 03 section 3.13), which is about 71 µs of CPU for each read. 10x is about 7 µs of server CPU for each read with pipelining. The floor of a round trip in the kernel is part of that, so document 06 must batch the socket work of pipelined statements.

### 19.12.4 Disk

The disk budgets are in document 02 section 2.6.3: below 9.45 GB at M4 and below 7.67 GB at M8, with summaries. `rupg_stat_file` gives the breakdown. Document 20 publishes it for each run so that a miss shows its cause.

## 19.13 Tests

Document 21 owns the test suites. These tests are the ones that this document requires:

1. **Budget enforcement.** With `rupg.memory_limit` at 64 MiB, every TPC-H query at SF1 completes with the correct answer, and `memory.peak` stays below 64 MiB plus the base of section 19.3.3.
2. **No page cache charge.** With `rupg.direct_io` on, a cold ClickBench run charges less than 64 MiB of page cache to the cgroup, from `file` in `memory.stat`.
3. **Spill cleanup.** After a crash in the middle of a spilling query, recovery leaves no spill extent allocated, and the file size after a checkpoint is the size before the query.
4. **Idle session.** 10,000 idle connections raise `memory.peak` by at most 640 MiB.
5. **Idle server.** An idle server with 1,000 idle sessions uses less than 1 ms of CPU each second, from `usage_usec`.
6. **Background share.** Under a constant analytic load on all workers, the mover keeps the hot store of a table that receives 10,000 inserts a second below 4 segments' worth of rows.
7. **Settings.** Every PostgreSQL setting of section 19.4 gives the PostgreSQL value through `SHOW`, `pg_settings` and `current_setting`, and each version shim of document 05 gives the value of its version.
