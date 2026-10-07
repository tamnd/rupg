# 15. Optimizer

Written 7 October 2026. PostgreSQL planner facts are from the PostgreSQL 18 documentation and the PostgreSQL 19 beta 4 source tree. Join research figures are from the papers named in each section.

This document specifies how rupg turns a query tree into a physical plan. It covers the passes, the rewrites, the statistics, cardinality estimation, join ordering, predicate transfer, the choice of access paths and physical operators, `EXPLAIN`, the plan cache, and the reasons why rupg has no learned component. The analyzer of document 07 produces the query tree. The executor of document 14 runs the plan. The crates are `rupg-plan`, which holds the logical and physical plan types and their text forms, and `rupg-opt`, which holds the passes. Both are at rank 9, with `rupg-analyze`.

The design position is the one of `../2140/09-optimizer.md`: the job of the optimizer is to avoid a very bad plan, not to find the best one. On ClickBench the plans are nearly fixed, because there is one table. On JOB, TPC-H and TPC-DS a bad join order can cost three orders of magnitude, and the cause is almost always a wrong cardinality estimate that grows through the join tree. So rupg spends its effort on plans that survive wrong estimates: predicate transfer, exact reduction and decisions that the executor takes at runtime.

## 15.1 The shape

The optimizer is a fixed sequence of passes. Each pass is a pure function from a plan to a plan. The sequence is: logical rewrites, statistics binding, join ordering, predicate transfer planning, access path choice, physical planning, and pipeline marking.

**Each pass can be turned off by name.** The setting `rupg.disable_passes` takes a list of pass names. This is a test feature first. When a differential test of document 21 finds a wrong answer, the harness turns passes off one at a time to find the pass that caused it.

**Each pass keeps an invariant.** In debug builds, after each pass, a check verifies that types are consistent, that every column reference resolves, that correlated references are bound, and that the output columns are the same. A pass that breaks the invariant fails at once.

**Each plan has a text form.** The logical plan and the physical plan print as text and parse back. A plan test can then state the input plan of one pass by hand and compare the output with an expected text. This is the rule of document 04 that every layer has a text form.

## 15.2 Logical rewrites

PostgreSQL applies some rewrites in the analyzer and the rewriter, before the planner: view expansion, rule application and row level security quals. rupg does these in the analyzer of document 07, in the same order, because the order is visible in results when a rule or a policy has a side effect. The optimizer starts after them.

| Rewrite | Notes |
|---|---|
| constant folding and expression simplification | only for `IMMUTABLE` functions, as PostgreSQL does; `STABLE` functions fold at execution start |
| subquery unnesting | dependent join elimination of Neumann and Kemper (BTW 2015), with the extensions of Neumann (BTW 2025) |
| `IN` and `EXISTS` to semi-joins, `NOT EXISTS` to anti-joins | `NOT IN` becomes an anti-join only when both sides are proven not null; otherwise a null-aware anti-join |
| outer join reduction | a left join becomes inner when a filter above rejects nulls of the right side |
| filter and projection pushdown | down to the scan, where the filter becomes a summary test (document 12 section 12.6) |
| `OR` to `= ANY` | the rewrite of PostgreSQL 18, so an index and a code set can serve the list |
| self-join elimination | on unique keys, controlled by `enable_self_join_elimination`; the release that added it to PostgreSQL must be confirmed at M3 |
| distinct elimination | when a unique key or an aggregate below already makes the rows distinct |
| aggregate pushdown below joins | when functional dependencies allow it |
| limit pushdown | into sorts, which become top-N, and into scans, which stop early |
| partition pruning | at plan time for constants and at run time for parameters, as PostgreSQL does |
| CTE inlining | a CTE referenced once and not `MATERIALIZED` is inlined, as in PostgreSQL 12 |

**Unnesting is where wrong answers hide.** A correlated subquery with nulls, `NOT IN` with a null in the list, and an aggregate over an empty group each have a result that a careless rewrite changes. Each rewrite of this kind has a test file in document 21 that compares rupg with PostgreSQL 19 on generated data with nulls.

**Functional dependencies are proved here.** Document 12 section 12.8 lets a group key that is an injective function of one column use that column's synopsis. Q35 groups by `ClientIP, ClientIP - 1, ClientIP - 2, ClientIP - 3`, and Q34 groups by a constant and `URL`. The rewrite pass proves that each extra key is a function of the first, marks the dependency in the plan, and the group by then hashes the first key only. The proof uses a fixed set of rules: a constant depends on anything, an expression of one column with `IMMUTABLE` functions depends on that column, and an injective expression such as `x - 1` on an integer that cannot overflow determines `x` back. A key that the rules cannot prove is not marked.

## 15.3 Statistics

### 15.3.1 PostgreSQL's statistics

Tools read `pg_statistic` through the `pg_stats` view, and `pg_dump` in PostgreSQL 18 writes statistics with `pg_restore_relation_stats` and `pg_restore_attribute_stats`. rupg must produce the same view with the same columns and the same meaning: `null_frac`, `avg_width`, `n_distinct`, `most_common_vals`, `most_common_freqs`, `histogram_bounds`, `correlation`, and the element statistics for arrays and `tsvector`. `ANALYZE` computes them from a sample of `300 × default_statistics_target` rows, which is PostgreSQL's sample size, and `default_statistics_target` is 100 by default.

`CREATE STATISTICS` with `ndistinct`, `dependencies`, `mcv` and expressions works, and `pg_stats_ext` shows the results. The restore functions of PostgreSQL 18 load values into the same tables. The planner uses them as PostgreSQL does, for the selectivity of conjunctions and for the group count of multi-column keys.

**`pg_class.reltuples` and `relpages` are kept current.** PostgreSQL updates them at `VACUUM` and `ANALYZE`. Tools read them as a fast row count. rupg updates `reltuples` from the segment row counts and the hot store count, and `relpages` from the table's bytes divided by 8,192, so that a tool that multiplies `relpages` by the block size gets the real size.

### 15.3.2 rupg's own statistics

PostgreSQL statistics are a sample, taken at `ANALYZE`. rupg has better sources, and the optimizer uses them first.

| Source | What it gives | Exact |
|---|---|---|
| segment summaries (document 09 section 9.7) | row count, null count, minimum, maximum, code set of each segment | yes, after the delete correction |
| table synopses (document 12 section 12.8) | the most frequent values with exact counts and a bound for the rest | yes |
| a HyperLogLog sketch for each column of each segment | the distinct count, mergeable across segments | no, about 1.6 percent standard error at 4,096 registers |
| an AMS sketch for each join key column with a link or an index | the second frequency moment, for join size bounds | no |
| a reservoir sample of 65,536 rows for each table, kept by the mover | correlated filters, `LIKE` selectivity | no |

The HyperLogLog error is the standard error of 1.04 divided by the square root of the register count (Flajolet, Fusy, Gandouet and Meunier, 2007). The sketches are new parts of the segment summary, built by the mover, and they count in the summary bytes of document 12 section 12.10.

**`ANALYZE` reads these, not a new sample.** `ANALYZE` on a table builds the `pg_stats` rows from the summaries, the synopses and the reservoir sample. It does not scan the table. Its run time is then proportional to the number of segments, not rows. `autovacuum` runs it when the mover has written new segments.

## 15.4 Cardinality estimation

**Filters on one table use the most exact source that applies.** A filter that the summaries decide for a whole segment (all rows pass or none pass) gives an exact count for that segment. For the segments that remain, a range filter uses the vector zone maps and the histogram, an equality on a frequent value uses the synopsis, and a conjunction of predicates on correlated columns is evaluated on the reservoir sample. The sample avoids the independence assumption, which is the largest source of error on one table. Filters on the hot store use the sample too.

**Joins use distinct counts and bounds.** A join size estimate uses the containment assumption with the HyperLogLog distinct counts. The AMS sketch gives an upper bound on the size of an equi-join: the size is at most the square root of the product of the two second moments, by the Cauchy and Schwarz inequality. The optimizer keeps both the estimate and the bound. The executor reserves memory from the bound, so that a wrong estimate costs time, not a failure.

**The estimates are wrong, and the plan must still be good.** Every study since the Join Order Benchmark paper reports errors of several orders of magnitude on multi-join queries. Leis et al. revisited this in "Still Asking: How Good Are Query Optimizers, Really?" (PVLDB 18(12), 2025). rupg does not try to fix estimation. It reduces the cost of a bad estimate in three ways: predicate transfer (section 15.6) makes join order matter less, the executor chooses aggregate strategy and join partitioning at runtime from observed sizes (document 14), and memory reservations use bounds.

## 15.5 Join ordering

**The enumerator is DPhyp.** Moerkotte and Neumann's DPhyp (SIGMOD 2008) enumerates connected subgraphs of the join hypergraph, so outer joins and predicates over three or more tables are handled correctly. It builds bushy plans. It runs up to `rupg.join_dp_limit` relations in one join block, with a default of 14, which is a budget for planning time to be measured at M3 on JOB, where the largest query joins 17 tables. Above the limit, a greedy method joins the pair with the smallest estimated result first, as in GOO (Fegaras, 1998), and then DPhyp improves windows of the greedy plan.

**PostgreSQL's join settings keep their meaning.** Users force a join order by writing explicit `JOIN` syntax with `join_collapse_limit = 1`, and they limit subquery flattening with `from_collapse_limit`. rupg honours both exactly as PostgreSQL does, because users and tools use them as hints. `geqo` and `geqo_threshold` (12 by default) are accepted: when `geqo` is on and a block has at least `geqo_threshold` relations, rupg uses the greedy method instead of DPhyp. The genetic search itself is not implemented, so the `geqo_*` tuning settings have no effect.

**The cost model is simple.** It counts the rows that each operator produces and the bytes that it reads, weighted by whether a hash table or a sort fits in the cache. It has no per-tuple constants for each operator type. A detailed cost model fed with wrong cardinalities is not more accurate, only more confident. PostgreSQL's cost settings (`seq_page_cost`, `random_page_cost`, `cpu_tuple_cost` and the others) are accepted and shown with PostgreSQL's defaults. Only `random_page_cost` and `seq_page_cost` act, through their ratio, on the choice between an index scan and a scan of cold rows that are not in the buffer pool.

**Join order matters less after transfer.** RPT (SIGMOD 2025) measured the ratio of the worst join order to the best at 1.6x after predicate transfer, with a geometric mean of about 1.5x. Without transfer the ratio can be orders of magnitude. So rupg's enumerator does not need to be exhaustive above the limit. It needs to avoid the plans that transfer cannot rescue, which are plans with a cross product or with a large build side under a selective probe side.

## 15.6 Predicate transfer

Predicate transfer runs semi-join reductions over the join graph before the joins, so that each table is reduced to the rows that can join. It is in M6 with the link index, and its Bloom filter form is in M3 for TPC-H.

**The transfer plan is a spanning tree.** The optimizer builds a spanning tree of the join graph, rooted at the largest table, which is RPT's LargestRoot rule. A forward pass sends filters from the leaves to the root, and a backward pass sends them from the root to the leaves. Each table receives filters from its neighbours and applies them in its scan. For an acyclic query this is a full Yannakakis reduction when the filters are exact.

**Only safe edges transfer.** An edge on the null-producing side of an outer join, an edge under an aggregate, and an edge from a correlated subquery that the rewrites could not unnest can change the result if a filter crosses it. RPT's SafeSubjoin rule identifies the edges that are safe. rupg applies it, and an edge that the rule does not prove safe does not transfer.

**The filter form is chosen for each edge.**

| Edge | Filter | False positives | Cost per row |
|---|---|---|---|
| along a link (document 13) | bitmap of row ids | none | one gather and one bit test |
| between two coded columns with one dictionary | bitmap of codes | none | one bit test |
| between two integer columns with a known small range | bitmap over the range | none | one bit test |
| any other | blocked Bloom filter | about 1 percent at 10 bits for each key | one hash and one cache line |

RPT reported the cost of its Bloom filters at 28 percent of query time on TPC-H, 12 percent on JOB and 46 percent on TPC-DS. Exact filters remove the hash and the false positives, so the edges that use them cost less. The exact forms are the reason that rupg can expect the Yannakakis+ (PACMMOD 2025) and Shredded Yannakakis (PVLDB 18) results: one query from 488 s to 13.2 s, and faster on 85.3 percent of 1,849 queries with up to 62.5x. RPT+ (VLDB 2026) improved RPT by 1.47x on JOB and 1.17x on TPC-H, and its changes to the transfer schedule apply to rupg's plan in the same way.

**Transfer is skipped when it cannot pay.** A query on one table, a join of two tables where the probe side has no filter, and a query whose estimated work is below a threshold get no transfer. ClickBench has no joins and pays nothing.

## 15.7 Access paths

For each table in the plan, the optimizer chooses how to read it. The choices and their order of preference for a filter that selects few rows are below. The optimizer computes a cost for each choice that applies and takes the lowest.

**Summary answer.** When the query is an aggregate that document 12 section 12.6 can answer from summaries, the plan reads summaries for the segments that pass in full, reads the other segments, and scans the hot rows. This covers C1 and C8.

**Point path.** When the analyzer marked the statement as a point statement (document 14 section 14.9), the optimizer checks that the unique index exists and is valid, and emits a point program next to the plan.

**Index scan.** A B-tree, hash, GiST, SP-GiST or GIN index that serves the filter. For a B-tree in the sparse form (document 12 section 12.3), the cost of the cold part is the number of vectors that the fences select. Several indexes on one table combine by a bitmap AND or OR of row ids, as PostgreSQL's `BitmapAnd` and `BitmapOr` do.

**Scan with summary pruning.** The default for an analytic query. The cost is the number of vectors that the zone maps and code sets cannot skip, estimated from the summaries.

**Certified top-N.** For `GROUP BY ... ORDER BY count(*) DESC LIMIT k` on a column with a synopsis, or with a functional dependency on one (section 15.2), the plan places the `Certified Top-N` operator of document 14 section 14.7 above the scan, with the full aggregate as its fallback.

**Gram pruning.** For `LIKE`, `ILIKE` and `~` on a string column, the plan attaches the gram summaries of document 12 section 12.9 to the scan, when the rules of that section allow them.

**Vector search.** For `ORDER BY v <-> $1 LIMIT k` with a vector index, the plan chooses the pre-filter scan, the filtered traversal or the post-filter of document 13 section 13.4.4 from the estimated selectivity of the other filters. The estimate uses the reservoir sample, because the filter and the vector are often correlated, for example a tenant filter.

**Scan form.** The plan states, for each column of a scan, the form in which the consumers want it: codes for a group by, a join or an equality, encoded data for a range filter, strings only for the output. This is the layout adaptation of `../2140/09-optimizer.md`. The executor can change it at runtime.

**Once-per-code marking.** The plan marks each expression that meets the rule of document 14 section 14.6, so that the executor builds the memo.

## 15.8 Physical choices

**Join algorithm.** For each join the optimizer chooses among the link join, the hash join, the merge join, the index nested loop and the nested loop. A link join is chosen when a complete or partial link exists and the plan reads the child side. A merge join is chosen when both inputs come sorted on the key, from sorted segments or an index. An index nested loop is chosen when the outer side is small by its bound, not only by its estimate. Otherwise the plan uses a hash join, with the smaller side by bound as the build side.

**Worst-case optimal joins are chosen per query.** For a cyclic join, the optimizer computes the size bound of the binary plan from the AMS sketches and the size bound of the WCOJ from the HyperLogLog counts, and chooses the WCOJ only when its bound is smaller by a factor of 4, which is a budget. Groß, ten Wolde and Boncz (CIDR 2025) measured WCOJ on triangle queries from 0.23x to 14.91x of binary plans, so a fixed choice in either direction loses on some queries. The plan uses factorized `expanded` vectors when an aggregate sits above a join that multiplies rows.

**Parallelism.** Every analytic pipeline runs on all workers through morsels. `max_parallel_workers_per_gather` keeps its meaning as a cap on the workers of one query: 0 means one worker, as PostgreSQL then runs without parallel workers. `parallel_setup_cost` and `parallel_tuple_cost` are accepted and ignored, because morsels have no setup cost of that size.

**The `enable_*` settings act as in PostgreSQL 18.** `enable_seqscan`, `enable_indexscan`, `enable_bitmapscan`, `enable_hashjoin`, `enable_mergejoin`, `enable_nestloop`, `enable_hashagg`, `enable_sort`, `enable_memoize` and the others make the planner avoid the operator when another plan exists, and PostgreSQL 18 counts disabled nodes rather than adding a large cost. rupg follows the PostgreSQL 18 rule, and `EXPLAIN` marks a disabled node the same way. rupg adds `rupg.use_summaries`, `rupg.use_links`, `rupg.synopsis_mode` and `rupg.predicate_transfer` with the same behaviour for its own operators.

**Runtime may change the plan.** The plan is a starting configuration. The executor changes aggregate strategy, join partitioning, filter order, compaction and compile tier at runtime (document 14). So the planner is written to be fast and reasonable, not slow and exhaustive.

## 15.9 EXPLAIN compatibility

`EXPLAIN` is an interface. People read it, tests compare it, and programs parse it. sqlx parses `EXPLAIN (VERBOSE, FORMAT JSON)` to infer the nullability of query columns. Monitoring tools parse `auto_explain` output. Plan visualizers parse the JSON form. So rupg must give a plan in PostgreSQL's vocabulary, even though its plan is different. Document 02 states what rupg does not claim: byte-identical `EXPLAIN` text. The regression tests that compare plan text are counted in their own denominator.

### 15.9.1 What must match

**The syntax and the options.** `EXPLAIN` accepts every option of PostgreSQL 19: `ANALYZE`, `VERBOSE`, `COSTS`, `SETTINGS`, `GENERIC_PLAN`, `BUFFERS`, `SERIALIZE`, `WAL`, `TIMING`, `SUMMARY`, `MEMORY` and `FORMAT` with `TEXT`, `JSON`, `XML` and `YAML`. `BUFFERS` is on by default with `ANALYZE`, as in PostgreSQL 18.

**The structure.** Each format has PostgreSQL's structure: in JSON, a list with one object with a `Plan` key, and each node with `Node Type`, `Parent Relationship`, `Startup Cost`, `Total Cost`, `Plan Rows`, `Plan Width`, the actual fields under `ANALYZE`, `Output` under `VERBOSE`, and child nodes under `Plans`. Join nodes have `Join Type` with PostgreSQL's values. Scan nodes have `Relation Name`, `Schema` and `Alias`. sqlx needs `Output`, `Join Type` and the tree shape, so these must be exact.

**The node names.** A rupg operator that has the same meaning as a PostgreSQL node uses the PostgreSQL name.

| rupg operator | PostgreSQL node name |
|---|---|
| scan of hot and cold rows, with summaries | `Seq Scan` |
| index scan, dense or sparse form | `Index Scan`, `Index Only Scan` |
| row id batch from several indexes | `Bitmap Index Scan`, `BitmapAnd`, `BitmapOr`, `Bitmap Heap Scan` |
| hash join with the unchained table | `Hash Join` with a `Hash` child |
| dense array or hash aggregate | `HashAggregate`, or `Aggregate` with `Strategy: Hashed` in JSON |
| aggregate from summaries | `Aggregate` over `Seq Scan` |
| top-N | `Limit` over `Sort`, with `Sort Method: top-N heapsort` |
| parallel pipelines | `Gather` or `Gather Merge` above the parallel part, with `Workers Planned` |
| point program | the nodes of the equivalent plan, such as `Index Scan` |

**Operators with no PostgreSQL meaning get new names.** A link join is not a hash join and not a nested loop. A certified top-N is not a sort. Giving them PostgreSQL names would make a reader believe a wrong thing about the plan. **Decision: rupg uses three new node names: `Link Join`, `Certified Top-N` and `Graph Search`.** A link join has a `Join Type` field with PostgreSQL's values, so sqlx's nullability inference works on it. A tool that does not know a node name treats it as a generic node, which is what tools do with each new node that PostgreSQL adds, such as `Memoize` in PostgreSQL 14.

The setting `rupg.explain_names` takes `rupg` (the default) or `postgres`. With `postgres`, `Link Join` prints as `Nested Loop` with an inner `Tid Scan`, `Certified Top-N` prints as `Limit` over `Sort` over `HashAggregate`, and `Graph Search` prints as `Index Scan`. The plan text tests of the regression suite run with `postgres`.

### 15.9.2 What differs and how it is shown

**Costs are rupg's costs.** The `cost=a..b` fields hold rupg's cost, scaled so that reading 8 KiB of a cold column sequentially costs 1.0. The magnitudes are then in the range that PostgreSQL users know, but they are not PostgreSQL's numbers, and a test must not compare them. Plan tests in the regression suite run with `COSTS OFF`, as most of them already do.

**rupg properties are extra fields.** rupg adds fields to nodes. Tools ignore fields that they do not know. The text form prints them only with `VERBOSE`, so the default text is close to PostgreSQL's. The fields are:

| Field | Node | Meaning |
|---|---|---|
| `Segments Skipped`, `Vectors Skipped` | `Seq Scan` | pruned by summaries |
| `Segments Answered`, `Rows Answered From Summaries` | `Seq Scan` | aggregated from summaries |
| `Hot Rows` | `Seq Scan` | rows read from the hot store |
| `Cold Part` | `Index Scan` | `dense` or `sparse` |
| `Transfer Filters` | scan nodes | each filter received by predicate transfer, with its form |
| `Link` | `Link Join` | the link index used, and `trusted` or `validated` |
| `Certificate` | `Certified Top-N` | `held` or `failed`, with the bound |
| `Memo Codes` | any node with a memo | the number of distinct codes evaluated |
| `Tier` | `Gather` | the compile tier of each pipeline at the end |
| `Adaptive` | any node | each runtime decision and the observation that caused it |
| `Point Path` | the top node | `true` when the point program ran |
| `Estimate Bound` | any node | the upper bound used for memory |

**Buffers are rupg pages.** `Buffers: shared hit=... read=...` counts rupg buffer manager pages of 16 KiB, and the `EXPLAIN` header states the page size when `BUFFERS` is on. `WAL` counts the bytes of the worker log records that the statement wrote.

**Estimates and actuals are printed together.** Under `ANALYZE` with `VERBOSE`, each node prints the ratio of actual rows to estimated rows. This is the first number that a person needs when they look at a bad plan.

### 15.9.3 auto_explain and pg_stat_statements

`auto_explain` from `rupg-contrib` writes plans to the log with its PostgreSQL settings (`auto_explain.log_min_duration`, `log_analyze`, `log_format` and the others). `pg_stat_statements` records statements with the same query ID, which rupg computes with PostgreSQL's jumbling rules, so that a query has the same ID on rupg and on PostgreSQL 19. The jumbling walks PostgreSQL's query tree, so the ID matches only where rupg's analyzer builds the same tree. Section 15.14 keeps this open.

## 15.10 The plan cache

**`plan_cache_mode` keeps PostgreSQL's rule.** A prepared statement gets custom plans for its first five executions. After that, PostgreSQL uses a generic plan if its estimated cost is not much more than the average custom plan. `pg_prepared_statements` shows `generic_plans` and `custom_plans` counts, so the rule is visible and rupg follows it. `force_generic_plan` and `force_custom_plan` work as in PostgreSQL. A point program is always generic, because it has no plan choice that depends on a parameter.

**Plans depend on states, and the states are checked at execution.** A plan that uses a link, a synopsis or a sparse index part depends on the state of that structure. The plan records the state it assumed. At execution, a cheap check compares the recorded state with the current one. If a link changed from complete to partial, a plan that needs the complete form falls back to the alternative stored with it, or is planned again. Catalog changes invalidate plans as in PostgreSQL.

**The cache is in the memory budget.** The plan cache of each session and the shared cache of point programs reserve from `rupg.memory_limit`. When the reservation is refused, the least recently used plans are freed.

## 15.11 No learned optimizer

rupg has no learned cardinality estimator, no learned cost model and no learned join order. This section gives the reasons, because the question comes up.

**The evidence does not support it.** Lehmann et al. (PVLDB 2024) evaluated learned query optimizers and found that PostgreSQL's own optimizer beat them on the measures that matter in operation. "How Good are Learned Cost Models, Really?" (SIGMOD 2025) found the same for learned cost models used to choose plans. Leis et al. (PVLDB 18(12), 2025) revisited the Join Order Benchmark and found that the main problem is still cardinality estimation, and that robust execution reduces its effect.

**A learned component breaks properties that rupg needs.** A plan must be deterministic, so that a test gives the same plan on each run and a differential failure can be reproduced. A plan must be explainable in `EXPLAIN`. A model trained on queries also conflicts with the honesty rule of document 02 in spirit: the rule says that structures are built from the data and never from the queries, and a model trained on the workload is a structure built from the queries.

**rupg spends that effort on robustness.** Predicate transfer, exact reduction, runtime decisions in the executor and bounds for memory make the cost of a wrong estimate small. That is the same goal that learned optimizers have, reached with mechanisms that are deterministic and testable.

## 15.12 Distributed planning

In a cluster (document 18), the optimizer adds exchange points to the physical plan. A join whose two inputs are sharded on the join key runs in each shard with no exchange. A join with a replicated table runs in each shard. Any other join moves one side with a hash shuffle or a broadcast, chosen by the bound of the smaller side. An aggregate runs as a partial aggregate in each shard and a final aggregate after a shuffle on the group key, or after a gather when the group count bound is small. A certified top-N runs the certificate on the merged synopses of the shards. Document 18 section 18.8 gives the arithmetic of analytic scale-out. Predicate transfer filters cross the network in their exact or Bloom form, which is smaller than the rows that they remove.

## 15.13 Milestones and tests

| Milestone | Optimizer work |
|---|---|
| M3 | rewrites, PostgreSQL statistics, DPhyp, Bloom predicate transfer, access paths for B-tree and scans, the point path check, `EXPLAIN` in all formats, the plan cache |
| M4 | summary statistics, summary answers and pruning, scan form, `ANALYZE` from summaries |
| M6 | link joins, exact transfer, every index family, WCOJ choice, vector search strategies |
| M8 | certified top-N placement, functional dependency proofs, once-per-code marking, `Estimate Bound` |
| M11 | distributed planning |

**Plan tests.** Each pass has text-form tests. The JOB, TPC-H and TPC-DS queries run with each pass turned off in turn, and the answers must not change. The harness reports the estimation error of every join node on JOB as the q-error distribution, at each milestone, so that a change to estimation is visible.

**EXPLAIN tests.** A test parses the JSON `EXPLAIN` output of every query in the regression suite with sqlx's parser and checks that the nullability that sqlx infers is the same as on PostgreSQL 19. The plan text tests of the regression suite run with `rupg.explain_names = postgres` and `COSTS OFF`, and their pass rate is reported in its own denominator, as document 02 states.

## 15.14 Open questions

1. Whether the default `rupg.join_dp_limit` of 14 keeps planning time for the largest JOB queries under 10 ms. The limit is a budget with no measurement (section 15.5).
2. Whether `relpages` should report bytes divided by 8,192, which makes size arithmetic correct, or a page count that makes PostgreSQL's cost formulas in external tools behave as on PostgreSQL (section 15.3.1).
3. Whether the `pg_stat_statements` query ID can match PostgreSQL's for every statement (section 15.9.3).
4. Whether the three new `EXPLAIN` node names break a tool that matters, and whether `rupg.explain_names` should default to `postgres` for the wire server and `rupg` for the library (section 15.9.1).
5. Whether the WCOJ factor of 4 is right (section 15.8).
