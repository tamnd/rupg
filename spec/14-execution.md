# 14. Execution

Written 7 October 2026. PostgreSQL latency figures are from `../2140/compat/postgres/15-performance.md`. ClickBench figures are from document 02. rudb compile tier facts are from the `rudb-qc` crates.

This document specifies the executor of rupg: the vector, the pipelines and the scheduler, the scans of the two stores, work on dictionary codes, aggregation, joins, the point path, the HTAP interference target, the compile tiers, memory, and the PostgreSQL semantics that the executor must keep. The crates are `rupg-exec` for the operators, `rupg-func` for the functions and operators of the PostgreSQL surface, `rupg-jit` for the compile tiers and `rupg-exchange` for the operators that move data between nodes. All four are at rank 10. The kernels are in `rupg-kernels` at rank 2.

## 14.1 The shape

The executor is a push-based vectorized engine. A physical plan from document 15 is split at its pipeline breakers into pipelines. A pipeline has a source, a list of operators and a sink. The source produces vectors of up to 1024 values for each column, and each operator transforms them and pushes them to the next. A sink is a breaker: a hash table build, an aggregate, a sort, or the result sink of document 06.

**Two paths share one engine.** An analytic statement runs as pipelines over morsels on many workers. A short statement that matches section 14.9 runs on the point path, which is a short program with no pipelines and no morsels. Both paths use the same functions, the same row access and the same visibility rules. Only the control structure differs.

**The interpreter is the reference.** Every operator has a vectorized implementation in Rust that is the definition of its result. The compile tiers of section 14.12 must produce the same bits. A differential test runs each query on both and compares.

## 14.2 Vectors

A vector is a type, a length of up to 1024, a physical form, a validity mask and buffers. The forms are the forms of the rudb engine, extended by the rudb graph notes.

| Form | What it holds | Produced by |
|---|---|---|
| flat | one value for each row | a decoded column, an expression result |
| constant | one value for all rows | a literal, a parameter, a run that covers the vector |
| coded | a 32-bit dictionary code for each row and a reference to the global dictionary | a scan of a dictionary-coded column |
| sequence | a start and a step | a row id range, `generate_series` |
| encoded | bit-packed, frame-of-reference or run-length data not yet decoded | a scan, when the consumer can read the encoding |
| gathered | a source vector and a list of positions in it | a link join, a dictionary lookup with few distinct values |
| expanded | values and run starts, each value repeated by its run length | a backward traversal over a link |

**Validity has three cases**: all valid, with no mask, all null, with a flag, and a bitmap. Kernels for the first case have no null test.

**Strings are 16-byte views** with a length, a 4-byte prefix and a pointer, or inline up to 12 bytes, as in Umbra, DuckDB and Arrow's `StringView`.

**A filter produces a selection vector.** It does not copy the selected rows. An operator downstream reads the selection. When the selection is small, the next operator wastes its fixed cost on few rows. Data Chunk Compaction (SIGMOD 2025) found that 39 percent of DuckDB's chunks on JOB hold one row, and compaction gave up to 63 percent. So a join probe or a filter whose output selects fewer than a threshold of rows buffers its output and emits a full vector. The threshold is a constant measured at M3 on JOB, with a comment that names the run.

**`gathered` and `expanded` delay work.** A kernel on a `gathered` vector with a small source computes once for each source value and gathers the results. On an `expanded` vector, `sum` is each value times its run length, `count` is the last run start, and a predicate runs once for each value. Both are flattened in one place in the code, when an operator cannot read them.

## 14.3 Pipelines, morsels and the scheduler

**A morsel is the unit of work that a worker takes.** It is a range of vectors of one segment, or a range of row pages of the hot store, sized to run in about 1 ms. The source of a pipeline splits its input into morsels from the summaries, so a segment where most vectors are skipped becomes one small morsel.

**The scheduler has four queues** on each worker, in this order: I/O completions and short statements, analytic morsels, maintenance, idle. A worker takes a morsel from queue 2 only when queue 1 is empty. Section 14.11 gives the rule that bounds the wait of a short statement behind a morsel.

**Workers steal** morsels from other workers' queue 2 when their own queues are empty, from the same NUMA node first.

**Pipelines form a graph.** A probe pipeline waits for its build pipeline, and independent pipelines run at the same time.

**Cancellation is checked between vectors.** A `CancelRequest`, `statement_timeout`, `lock_timeout` or a terminated session sets a flag that the pipeline loop reads after each vector. The statement fails with SQLSTATE `57014` for a cancel or a statement timeout, as in PostgreSQL. A cancel never waits for a morsel to finish.

## 14.4 Scans

A table scan has two sources: the cold segments and the hot store. It reads both and its output is one stream of vectors. The order of rows is not defined, as in PostgreSQL without `ORDER BY`.

### 14.4.1 The cold store

For each segment visible to the snapshot, the scan does these steps:

1. **Test the summaries** (document 12 section 12.6). Skip the segment, mark it as passing in full, or go on.
2. **Answer from summaries** where the plan asks for it and the segment passes in full.
3. **Test each vector's zone map** and build the list of vectors to read.
4. **Evaluate the filter on the filter columns first**, on encoded data where the kernel can: a range test on frame-of-reference data, an equality test on codes, a run-length test once for each run.
5. **Read the other columns only for the selected rows.** This is late materialization. A column that no selected row needs is not decoded.
6. **Apply the delete mask** for the snapshot (document 10 section 10.5), as one more selection.

**Visibility in the cold store is cheap.** A segment is visible as a whole if its commit timestamp is at or below the snapshot. A deleted row is visible if its delete committed after the snapshot. So the scan needs one comparison for each segment and one mask for each vector. It never reads undo records.

### 14.4.2 The hot store

The hot store is a B+tree of 16 KiB row pages in row id order. The scan reads each page, checks the version of each row against the snapshot, follows undo records for rows changed after the snapshot, and writes the visible values into column vectors. This transposition is the cost of the hot store. It is acceptable because the mover keeps the hot store small.

The filter runs after the transposition. Hot pages have no summaries, except user-created BRIN (document 12).

### 14.4.3 Index scans

An index scan returns batches of row ids. When the index order is not needed, the executor sorts each batch by location, which is PostgreSQL's bitmap heap scan in vector form. A cold row id is resolved through the row id to segment map (document 10 section 10.2), and row ids in one vector share one decode.

## 14.5 Work on codes

H2 of document 02 states that string keys are integer codes. The executor uses this in four places.

**Group by on a coded column** uses the code as the group index. When the dictionary has at most 2^20 entries, which is a constant measured at M4, the aggregate state is a dense array indexed by code, with no hash. Above that, the code is the hash key, which is 4 bytes, not the string.

**Distinct and `count(DISTINCT)`** on a coded column use a bitmap over codes.

**Equality filters and `IN` lists** are translated to codes once at the start of the scan. A literal string that is not in the dictionary matches no row, and the scan of that column ends at the summaries.

**Joins on coded columns** compare codes when both sides use the same dictionary, which is true for a self-join and for two columns of one table. Two tables have two dictionaries (document 09 section 9.5). For a join between them, the executor builds a translation array from the smaller dictionary's codes to the larger one's codes once, and then joins on codes.

**Sort on a coded column** sorts by code only when the dictionary is in collation order. Otherwise it sorts the distinct strings that occur, assigns ranks, and sorts by rank. Document 09 states whether a dictionary is kept in order.

## 14.6 Functions evaluated once per dictionary code

H6 of document 02 states that a function of one coded column runs once for each distinct value. Class C7 depends on it: Q27 computes `length(URL)` for each row, and Q28 computes `regexp_replace(Referer, '^https?://(?:www\.)?([^/]+)/.*$', '\1')` for each row and groups by the result. In the DuckDB run that `../2140/08-codegen.md` analyzed, Q28 alone was 24.7 percent of DuckDB's total time, and in the Umbra run it was the slowest query at 1.393 s.

### 14.6.1 The rule

An expression qualifies when all of these are true:

1. It reads exactly one column, and that column is coded in the vector.
2. Every other input is a constant or a parameter of the statement.
3. Every function and operator in it is `IMMUTABLE` or `STABLE`. A `VOLATILE` function, such as `random()` or `nextval()`, never qualifies.
4. Every function in it is built in, or is a SQL function that the analyzer inlined into built-in functions. A PL/pgSQL function never qualifies, because it can raise a notice or write a row, and the number of those events would change.

The executor then keeps, for each qualifying expression in a statement, a memo: an array indexed by code that holds the result, and a bitmap that tells which codes are computed. For each vector, it collects the codes of the selected rows that are not yet computed, evaluates the expression on their strings as one vector, writes the memo, and then gathers the results for the vector's rows by code.

**The memo is filled on demand, not from the whole dictionary.** This rule keeps PostgreSQL's error behaviour. A dictionary can hold strings that no visible row has, because of deletes, and strings that no selected row has, because of the filter. If the executor evaluated `CAST(s AS integer)` on the whole dictionary, it could fail on a string that PostgreSQL never casts, with SQLSTATE `22P02`. On demand, the executor evaluates the expression only on codes that reach the expression in a row, which is exactly the set of strings on which PostgreSQL evaluates it.

**The result is coded when it is a string.** For Q28, the results of `regexp_replace` are strings, and the query groups by them. The memo assigns each distinct result a local code, so the group by downstream is a group by on local codes in a dense array. So the regular expression runs once for each distinct `Referer`, and the group by never hashes a string.

**The memo is shared by workers.** It lives in the statement's memory. A worker writes a result and then sets the bit with a release store. Two workers can compute the same code at the same time. Both get the same result, because the expression is deterministic, so the duplicate costs time but not correctness. The memo's size counts in the operator's memory reservation. When it does not fit, the executor evaluates for each row, which is slower and correct.

### 14.6.2 The cost

The cost becomes one evaluation for each distinct value that occurs, plus one gather for each row. Q28's cost then depends on the number of distinct `Referer` values that pass `Referer <> ''`, not on 100 million rows. Document 09 measures the distinct counts of the ClickBench string columns at M4, and section 14.16 lists the C7 measurement at M8. The regular expression engine is a DFA with literal prefilters, lifted from `rudb-regex`. It must match PostgreSQL's regular expression semantics, which are Spencer's ARE flavour, not Perl's. Document 24 keeps open whether the DFA covers enough of the ARE flavour for the regression suite (question 2).

**The rule extends to columns with few values in a vector.** A run-length or dictionary-encoded integer vector has few distinct values. A qualifying expression on it, such as `extract(minute FROM EventTime)` on a vector with many equal times, is evaluated once for each distinct value of the vector. This needs no memo across vectors.

## 14.7 Aggregation

**The strategy depends on the key.** A key with a known small range, from the summaries, uses a dense array: a coded string, a `smallint`, a date in a known range, a boolean. A key whose distinct count is unknown starts with a hash table for each worker. After a fixed number of morsels each worker reports its table size, and if the extrapolated group count passes a threshold, the aggregate switches to a partitioned table shared by all workers, as in "Global Hash Tables Strike Back". The switch is at runtime, because a group count estimate for two columns is a product of two estimates and is often wrong by an order of magnitude.

**Top-k by count uses the synopsis.** For `GROUP BY k ORDER BY count(*) DESC LIMIT n`, the plan of document 15 can put a `Certified Top-N` operator in front of the aggregate. It reads the candidate set and the bound from the synopsis (document 12), counts the candidates with a small hash set in a scan of the key column, and tests the certificate. If the certificate holds, it emits the answer. If not, it starts the full aggregate. A failed certificate costs one extra pass over the key column.

**Aggregate state has a fixed size where it can.** `count`, `sum`, `min`, `max` and `avg` on fixed-width types are 8 or 16 bytes. `sum` on `bigint` and `numeric` uses 128-bit integers until it overflows, and then a `numeric` accumulator, so the result is exact as in PostgreSQL. `avg` on an integer returns `numeric`, as in PostgreSQL. `count(DISTINCT)` uses a hash set for each group with small sets inline.

**PostgreSQL aggregate features are complete**: `FILTER`, `DISTINCT` and `ORDER BY` inside aggregates, ordered-set and hypothetical-set aggregates, and `GROUPING SETS`, `ROLLUP` and `CUBE`. User-defined aggregates run one row at a time, which is slow and correct.

## 14.8 Joins

### 14.8.1 Hash join

**The hash table is unchained.** Birler, Schmidt, Fent and Neumann (DaMoN 2024) measured their unchained table at 2x faster on average than the chained designs, and up to 20x. The table stores the build tuples contiguously, grouped by hash prefix, with a directory of offsets and a small Bloom filter in each directory slot. A probe tests the filter, then scans the contiguous run. The build has two passes: count each prefix, then place. No pointer chasing occurs during the probe.

**The probe splits lookup and expand.** Diamond Hardened Joins (PVLDB 17, 2024) separate the probe into a lookup that finds the match range and an expand that produces the output pairs. With the split, a join that multiplies rows can keep its matches as an `expanded` vector and pass it to an aggregate above without a flatten. The paper reported up to 500x on CE benchmark queries with this split, for queries whose intermediate results grow.

**The partitioning depends on the build size.** A build that fits in the L2 cache of the workers is not partitioned. A larger build is radix partitioned. The decision is at runtime, from the observed build size.

**Spilling uses the unified pool.** A build that does not fit its reservation spills partitions to the file through the buffer manager. Kuiper, Groß, Boncz and Mühleisen (PVLDB 18, 2025) showed that one memory pool for the buffer cache and the operators gives graceful spilling. rupg uses that design (section 14.13).

### 14.8.2 Predicate transfer and exact reduction

Before a multi-way join runs, the plan of document 15 can reduce each table to the rows that can join. **Predicate transfer** sends a filter from each table to its neighbours along a tree of the join graph, forward and then backward. RPT (SIGMOD 2025) showed that after transfer the ratio of the worst join order to the best was 1.6x, with a geometric mean of about 1.5x, so join order matters much less. It also reported that the Bloom filters cost 28 percent of the time on TPC-H, 12 percent on JOB and 46 percent on TPC-DS. RPT+ (VLDB 2026) reported 1.47x on JOB and 1.17x on TPC-H over RPT.

**rupg uses exact reduction where it can.** Where a link exists (document 13), the transfer through it is a bitmap of row ids, not a Bloom filter. It has no false positives, and it costs one gather and one bit write for each row. Where both sides of an edge are coded columns of the same dictionary, the transfer is a bitmap of codes. Only an edge with neither uses a Bloom filter. This cuts the filter cost that RPT measured. Yannakakis+ (PACMMOD 2025) brought one query from 488 s to 13.2 s with exact reduction, and Shredded Yannakakis (PVLDB 18) was faster on 85.3 percent of 1,849 queries and up to 62.5x.

### 14.8.3 Link join

A join along a link is a gather of the parent's columns by the forward link of each child row (document 13 section 13.3). An inner join drops the sentinel. The gathered vectors are `gathered` vectors when the parent columns come from few parents. A trusted link needs no check. A validated link compares the gathered key with the child key and probes the parent index for the rows that fail.

### 14.8.4 Other joins

**Worst-case optimal joins are chosen per query.** Groß, ten Wolde and Boncz (CIDR 2025) measured a WCOJ on triangle queries from 0.23x to 14.91x of a binary join plan. A WCOJ is right only for cyclic queries with a large intermediate result. Document 15 chooses it from HyperLogLog and AMS sketches. The executor implements a multiway intersection over coded columns or links with bitmap AND, in the style of rudb's graph notes.

**Merge join, nested loop join and index nested loop join exist** for sorted inputs, for non-equality and lateral joins, and for a small outer side with an inner index.

**Every join type is native.** Inner, left, right, full, semi, anti and mark joins each have their own implementation, not a rewrite onto an inner join with a filter, so that a semi-join stops at the first match.

## 14.9 The point path

A transactional workload sends short statements: read one row by key, update one row by key, insert one row. TPC-C and YCSB are made of them. For these statements the cost of a plan tree, an operator setup and a vector of 1024 values is larger than the work. The point path runs them as a short program.

### 14.9.1 The budget

The floor on `gpc` is 13 µs for `SELECT 1` over a Unix socket, and a PostgreSQL point read takes 35 µs, so PostgreSQL's own work is about 22 µs (document 02). The transactional target is 10x on server CPU for each operation, so rupg's work for a point read must be at most 2.2 µs. This is a budget, divided as follows, and each line is measured at M3 with the harness of document 20.

| Step | Budget |
|---|---|
| decode `Bind` and `Execute`, find the portal | 0.3 µs |
| plan cache lookup and the point program | 0.1 µs |
| primary key probe in the B-tree, pages in the buffer pool | 0.5 µs |
| row read, visibility check, undo if needed | 0.3 µs |
| encode `DataRow`, `CommandComplete` and `ReadyForQuery` | 0.3 µs |
| session scheduling, snapshot, transaction start and end | 0.5 µs |
| reserve | 0.2 µs |

Socket system calls are outside this budget, because the 13 µs floor includes them on both sides.

### 14.9.2 What qualifies

The analyzer marks a statement as a point statement when all of these are true:

1. It reads or writes one table. A partitioned table qualifies when the partition key is fixed by the key condition.
2. It is a `SELECT` with an equality condition on every column of a unique index, an `INSERT ... VALUES` of one row, an `UPDATE` or `DELETE` with an equality condition on every column of a unique index, or an `INSERT ... ON CONFLICT` with one row. `RETURNING` is allowed, including the `OLD` and `NEW` forms of PostgreSQL 18.
3. The table has no row level security policy, no rule and no trigger except the referential triggers of foreign keys, which the point path runs natively.
4. The target list and `SET` expressions use only functions that are not `VOLATILE`, apart from `nextval` and `now`, which the point path calls directly.
5. The statement does not use a cursor.

`SELECT ... FOR UPDATE` and `FOR SHARE` qualify, and the point path takes the row lock. Isolation levels are those of document 11. Under `SERIALIZABLE` the point path records the predicate lock on the key.

### 14.9.3 How it runs

A point statement has a point program instead of a plan tree: a list of steps in a small fixed instruction set, such as "probe index I with parameters 1 and 2", "lock row", "check visibility", "evaluate expression E on the row", "write column 3", "emit columns 1, 4 and 5". The program is built once when the statement is prepared and kept in the plan cache with the plan. Expressions in it are the same expression IR as the vectorized engine, evaluated on one row by the interpreter.

**The point path keeps every PostgreSQL behaviour of the statement.** It raises the same errors with the same SQLSTATE, fires the same referential actions, returns the same command tag, and counts the row in the same statistics views. If a step finds a case that the program does not handle, such as a row that a concurrent transaction locked and a lock wait that needs the full lock manager, the program calls the general code for that step. It never gives a different result from the vectorized path, and a differential test checks it.

**`EXPLAIN` shows the plan, not the program.** `EXPLAIN` of a point statement shows `Index Scan using t_pkey on t` and the other nodes that PostgreSQL shows, because tools and tests read it. The verbose form adds the property `Point Path: true` (document 15 section 15.9).

**PL/pgSQL statements use it too.** HammerDB drives TPC-C through stored procedures. The PL/pgSQL engine of document 16 prepares each SQL statement in a function body, and each statement that qualifies gets a point program. A TPC-C New-Order in a procedure then runs about 30 point programs with no plan trees.

## 14.10 Sort, window, limit and the rest

**Sort uses normalized keys.** Each key is encoded into bytes that compare with `memcmp`, including direction, null ordering and collation. A collation other than `C` uses the ICU sort key, or the libc `strxfrm` result for a libc collation, so the order is the order PostgreSQL gives. Narrow fixed-width keys use a radix sort. The sort is a parallel merge sort with spilling.

**Top-N does not sort.** `ORDER BY ... LIMIT n` keeps a heap of n rows for each worker and merges them. With sorted segments, the scan reads the segments in key order and stops when the heap's worst row is better than the next segment's minimum. This is how C6 meets its 30 ms budget.

**Window functions** run over sorted partitions, with one sort shared by window functions that share a partitioning. Every frame mode of PostgreSQL is supported: `ROWS`, `RANGE` and `GROUPS`, with `EXCLUDE`. PostgreSQL 19 adds `IGNORE NULLS` and `RESPECT NULLS` for `lead`, `lag`, `first_value`, `last_value` and `nth_value`, and rupg must support them.

**Set-returning functions in the target list** follow PostgreSQL 10's rules: functions in one target list run in lockstep, and the output has as many rows as the longest result.

**CTEs** follow PostgreSQL 12's rules: a CTE referenced once and not `MATERIALIZED` is inlined, and others are computed once. A recursive CTE runs as a loop over a working table, with the link traversal of document 13 when the plan chooses it.

**Cursors** hold their pipeline state and snapshot between `FETCH` calls. `SCROLL` and `WITH HOLD` cursors materialize as in PostgreSQL.

## 14.11 The HTAP interference target

rupg runs transactional and analytic statements on the same node, on the same data, at the same time. The value of this is lost if an analytic query makes transactions slow, or if transactions make analytic queries see stale data. This section sets the target for both.

### 14.11.1 The mechanisms

**A short statement waits at most one vector.** Document 04 states that a point statement waits at most one morsel, which is about 1 ms. That is 28 times PostgreSQL's whole point read. So rupg tightens it: the pipeline loop reads the worker's queue 1 flag after each vector. If the flag is set, the worker parks the morsel at the vector boundary and runs queue 1 until it is empty. The pipeline state is in the morsel task, so the park is a return from the loop and the resume is a call. A vector of 1024 values takes from about 1 µs for a simple filter to about 50 µs for a complex expression on strings, so the wait is at most about 50 µs. This is a prediction, and section 14.11.3 gives the measurement.

**Analytic scans do not evict transactional pages.** The buffer manager keeps two classes of pages: hot store and index pages, and cold segment pages. A scan of cold segments reads its pages in a ring that it reuses, so it evicts its own pages first. Hot store and index pages are evicted only by the memory pressure of other hot store and index pages, or by a budget that `rupg.memory_limit` sets for them.

**Analytic reads take no row locks.** A transaction never waits for an analytic query, except for DDL locks.

**Analytic queries see every committed transaction.** A query's snapshot is the newest commit timestamp at its start, and its scan reads the hot store as well as the cold store. So the freshness lag is zero by construction. There is no extract and load step.

**Long snapshots hold undo.** An analytic query that runs for 10 s keeps the undo records that its snapshot may need. Under a TPC-C load, these records grow. PostgreSQL 17 removed `old_snapshot_threshold`, so PostgreSQL has no way to cut a long snapshot either. rupg keeps undo in the memory budget and spills old undo to the file. A hot store scan that must follow long undo chains gets slower. Document 24 keeps open whether rupg copies the visible hot rows at the start of a long analytic query to cut the undo that it holds (question 3).

### 14.11.2 The target

The test runs two workloads on one node at the same time. The transactional workload is TPC-C through HammerDB with stored procedures, and separately YCSB workload B. The analytic workload is the CH-benCHmark analytic queries (Cole et al., DBTest 2011) on the TPC-C data, and separately ClickBench on a second table. Each is also run alone. HATtrick (Milkai et al., SIGMOD 2022) defines the throughput frontier of an HTAP system, and the report draws the frontier for the four pairs.

| Measure | Target |
|---|---|
| p99 latency of a transaction under analytic load | at most 1.2x its p99 without the analytic load, and at most 100 µs more |
| transactional throughput under analytic load, with transactions using at most half the cores | at least 0.9x of its throughput alone |
| analytic throughput, with transactions using a share u of CPU | at least 0.9 × (1 − u) of its throughput alone |
| freshness lag | zero: every query sees every commit before its start |

These are budgets that we set. They are not measurements. They are a target at M8, measured with the gates G7 to G10, and document 24 keeps open whether they become a gate of their own (question 4).

### 14.11.3 The measurement

The harness of document 20 measures the wait in queue 1 directly: each worker records the time from the queue 1 flag to the start of the short statement, as a histogram. The p99 of that histogram must be at most 50 µs under ClickBench load. If it is not, the vector boundary check is not enough, and the fix is a time check inside long kernels, such as the regular expression of Q28.

## 14.12 Compile tiers

The compile tiers are lifted from `rudb-qc` into `rupg-jit` at M6. They are in a late milestone on purpose. The Bespoke OLAP ablation (VLDB 2026) measured specialized code with a flat layout at 1.26x on TPC-H and 0.57x on CEB, and a specialized layout at 12.35x and 51.40x. Code is not where the 10x comes from. Layout and summaries are.

**There are three tiers**, as in `rudb-qc`:

| Tier | rudb crate | What it does | Compile cost |
|---|---|---|---|
| interpreter | `rudb-qc-interp` | runs the QIR of a pipeline with vector kernels | none |
| direct | `rudb-qc-direct` | emits x86-64 machine code in two linear passes, with no dependencies | microseconds (a prediction) |
| Cranelift | `rudb-qc-clif` | lowers QIR to Cranelift IR and compiles it | milliseconds |

QIR is the IR of `rudb-qc-ir`: typed, in SSA form with block parameters, with an explicit null model and a text form that round-trips. The direct tier follows the idea of TPDE (arXiv 2505.22610), which compiles 8x to 24x faster than LLVM at `-O0`, and of Umbra's Flying Start (VLDB Journal 2021). LLVM is not used. InkFuse (ICDE 2024) found that Umbra's LLVM path still costs about 20 ms for each query, which is about half of the average ClickBench query budget of 40.8 ms in document 02.

**A pipeline starts in the interpreter.** After it has processed a threshold of rows, the executor queues a compile job in queue 3. The direct tier compiles first. A pipeline that is still running after the Cranelift threshold gets a Cranelift compile. The pipeline switches tiers at a morsel boundary. A query that ends before the compile never pays for it. The thresholds are constants measured on ClickBench and TPC-H at M6, with a comment that names the run.

**All tiers give the same bits.** The rule of `rudb-qc` holds: overflow checks, null propagation, string comparison and floating point order must match the interpreter. No tier may reassociate floating point operations or use fused multiply-add unless the interpreter does. A difference here makes an answer change when the query gets faster, which is the worst class of bug.

**`jit` is a PostgreSQL setting and it controls nothing in rupg.** PostgreSQL 19 turns `jit` off by default. Clients and guides set `jit = off` to avoid PostgreSQL's LLVM compile latency. rupg's tiers compile in the background and never add latency, so `jit` and the `jit_*` settings are accepted, shown with PostgreSQL 19's defaults, and ignored. The tiers are controlled by `rupg.compile`, with values `auto` (the default), `interpreter` and `direct`.

**Only `rupg-jit` contains `unsafe` for the code arena**, as document 22 lists. The generated code is registered with the `perf` JIT interface on Linux, so a profile names the query.

## 14.13 Memory, spilling and limits

**Each operator reserves memory before it uses it.** The reservation is taken from `rupg.memory_limit`, which is shared by the buffer pool, undo, operators, the plan cache and the catalog cache (document 04). `work_mem` is the reservation that one operator gets before it must spill, as in PostgreSQL. `hash_mem_multiplier` multiplies it for hash tables, as in PostgreSQL.

**Every breaker spills.** Hash aggregate, hash join, sort and distinct spill partitions through the buffer manager into free pages of the file. A spilled partition is compressed with a fast encoding and processed alone. The pages are freed at the end of the statement and are never part of a checkpoint. `temp_file_limit` caps the spilled bytes of a session, and passing it fails the statement as in PostgreSQL.

**Admission control** queues an analytic query that cannot get its minimum memory. A short statement is never queued.

**An idle session holds at most 64 KiB** (document 06 section 6.4). The executor allocates the state of a statement in a region of the session that is freed at the end of the statement.

## 14.14 PostgreSQL semantics the executor must keep

Vectorized execution evaluates expressions on many rows at once, and adaptive execution changes the order of work. Neither may change an answer or an error.

**An error must not come from a row that PostgreSQL never evaluates.** A filter selects rows before the next expression runs, so `WHERE x <> 0 AND 10 / x > 1` evaluates the division only on rows where `x <> 0`. Conjuncts are evaluated in the order of the plan, and the adaptive reordering of filters is allowed only among conjuncts whose functions cannot raise an error. Each function in `rupg-func` carries a flag that states this. A comparison of integers cannot fail. A division, a cast from text and a regular expression can.

**`CASE` and `COALESCE` evaluate lazily.** A `CASE` branch runs only on the rows that its condition selects, as a selection vector. This keeps PostgreSQL's documented rule that `CASE` guards an expression.

**Errors carry PostgreSQL's SQLSTATE and message.** Division by zero is `22012`, numeric overflow is `22003`, an invalid input syntax is `22P02`, and so on. When a vector holds several failing rows, the executor reports the first in the order of the selection. PostgreSQL could report another row's error first, because its order is the scan order, so the test suite compares the SQLSTATE and the message pattern, not the failing value.

**Floating point follows PostgreSQL.** `NaN` equals `NaN` and sorts above every other value, `-0.0` equals `0.0`, and overflow in `float8` arithmetic raises `22003` where PostgreSQL raises it.

**Volatile functions run once for each row.** `random()`, `clock_timestamp()` and `nextval()` are evaluated for each row that reaches them, never once for a vector.

**New statement forms are complete.** `MERGE`, `RETURNING OLD` and `NEW` of PostgreSQL 18, and `INSERT ... ON CONFLICT DO SELECT` of PostgreSQL 19 are executor work, and they must give PostgreSQL's rows and command tags.

## 14.15 The exchange

`rupg-exchange` moves vectors between nodes by hash shuffle, broadcast and gather. Inside one node there is no exchange, because morsels are the unit of parallel work. Coded columns cross the network as codes plus the dictionary entries that the receiver lacks. Document 18 section 18.8 gives the scale-out arithmetic.

## 14.16 Milestones and tests

| Milestone | Executor work |
|---|---|
| M3 | the vector forms, scans of both stores, filters, projection, hash aggregate, hash join, sort, top-N, the point path, the interpreter |
| M4 | work on codes, summary answers, late materialization, filters on encoded data |
| M5 | `SERIALIZABLE` predicate locks in the point path, row locks |
| M6 | the compile tiers, link joins, exact reduction, merge join, WCOJ |
| M7 | PL/pgSQL point programs, triggers in the vectorized path |
| M8 | once-per-code functions on 100 million rows, the certified top-N on C3, the HTAP target, spilling under the unified pool |

**Differential tests are the main tests.** Each suite query of document 21 runs with each adaptive decision pinned both ways, on each tier, with and without the point path and the summaries, and must give one answer.

**The measurements for M8** are: the C7 times with and without the memo of section 14.6, the C3 certificate results of document 12, the queue 1 wait of section 14.11.3, and the point path budget lines of section 14.9.1.

## 14.17 Open questions

1. Whether the threshold for data chunk compaction is one constant or one for each operator (section 14.2).
2. Whether the DFA regular expression engine covers enough of PostgreSQL's ARE flavour, or a fallback engine is needed for back references and lookahead (section 14.6.2).
3. Whether a long analytic query copies the visible hot rows at its start, to release undo early (section 14.11.1).
4. Whether the HTAP target of section 14.11.2 becomes a gate, and with which numbers.
5. Whether the point path budget of 2.2 µs holds on macOS and in WebAssembly, where `io_uring` is not available (section 14.9.1).
