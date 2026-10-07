# 13. Graph indexes

Written 7 October 2026. pgvector facts are from the pgvector 0.8 README. rudb graph facts are from `../2140/graph/`. Byte counts at TPC-H SF100 are from `../2140/graph/03-the-file-format.md`.

This document specifies the three graph families of rupg. The link index stores a join between two tables as a function from row ids to row ids. The vector index stores a proximity graph over embeddings for nearest neighbour search, with the SQL surface of pgvector. Graph queries walk links for recursive queries and, when a setting turns it on, for property graph syntax. **Property graph syntax is a rupg addition and it is off by default.** PostgreSQL 19 has no property graph syntax, because SQL/PGQ was removed in PostgreSQL 19 beta 4, so a PostgreSQL client never needs it. The link and graph code is in `rupg-graph`, lifted from `rudb-graph`. The vector code is in `rupg-ann`. Both are at rank 6.

## 13.1 The idea

An equi-join on a foreign key is a function. Each child row has at most one parent row. A hash join computes that function again for each query: it hashes the parent keys, builds a table, and probes it with each child key. A link index stores the function once, as the parent's row id in a hidden column of the child. A join over a link is then a gather by position, with no hash and no probe.

The rudb graph notes split this into three parts: dense row ids, stored links, and execution that uses them. rupg has the first part already, because every row has a row id that does not change for its life. The second part is this document. The third part is document 14 section 14.8.

**Deleting every graph structure changes no answer.** This is the invariant of the rudb graph notes, and it is rule 4 of the honesty rule in document 02. A link is an access path. If it is missing, stale or ignored, the optimizer uses a hash join and gets the same rows. Section 13.6 gives the one exception, which is the vector index, and the reason for it.

**rupg differs from rudb in one way that matters.** rudb updates a row as a delete plus an append, so a row's position changes and every link to it must change. rupg updates in place and keeps the row id. A link to a parent stays valid when the parent is updated, and it stays valid when the mover moves the parent to the cold store. Only a change to the foreign key column of the child, or to the key of the parent, changes the function. Section 13.2.4 gives the rules.

## 13.2 The link index

### 13.2.1 How a link is created

A link is created in one of two ways. No link is created from queries.

**By a foreign key.** When `rupg.auto_link` is on, which is the default, each foreign key constraint creates a link on the child columns, provided that the link fits in the budget of section 13.2.6. The link is an index in the catalog with access method `link` and the name `<constraint>_link`. It is hidden from `\d` and `pg_indexes` when `rupg.show_extension_am` is off, because PostgreSQL would show no such index.

**By DDL.** `CREATE INDEX li_ord ON lineitem USING link (l_orderkey) WITH (parent = 'orders')` creates a link with no foreign key. The parent table must have a unique index or a primary key on the referenced columns. The storage parameter `parent` names the table, and `parent_columns` names the columns when they are not the primary key. This form is for schemas that declare no foreign keys. The grammar is PostgreSQL's `CREATE INDEX` grammar, so the statement parses on PostgreSQL too, where it fails because the access method does not exist.

### 13.2.2 The parts

A link has up to four parts, as in rudb. Each part is an optional section of the table's extent in the file.

| Part | What it maps | Form | Size at SF100 (rudb figures) |
|---|---|---|---|
| key map | parent key to parent row id | identity, dense offset bitmap with rank, or sorted values with a permutation | identity is 24 bytes |
| forward link | child row id to parent row id | bit-packed hidden column of the child, with a sentinel for no parent | `lineitem` to `orders`: 2.10 GB at 28 bits |
| monotone link | the same, when both tables are in key order | one bit vector of child plus parent rows, with rank and select | about 106 MB, replacing 2.10 GB plus 2.25 GB |
| backward adjacency | parent row id to its child row ids | CSR: offsets as a bit vector with select, neighbours bit-packed | `partsupp` to `part` monotone: about 14 MB |

**The forward link stores a local row number.** A rupg row id is 64 bits, with the shard in the high 16 bits. A link is always inside one shard (section 13.7), so the forward link stores only the low 48 bits, bit-packed to the width that the parent's largest local number needs. `orders` at SF100 has 150,000,000 rows, so the width is 28 bits, as in rudb. The sentinel for no parent is the largest value of that width.

**The monotone link replaces two columns.** When the child is sorted by the foreign key and the parent by its key, as `lineitem` and `orders` are after a TPC-H load sorted by order key, the link is one bit vector of 600,037,902 + 150,000,000 bits. The forward function is `rank0(select1(child))`. The backward range of a parent p is from `select0(p) - p` to `select0(p + 1) - p - 1`. This vector is about 106 MB with its rank and select directories, and it replaces the 2.10 GB forward link and a 2.25 GB backward structure. It also makes the foreign key column itself redundant for joins, but rupg keeps the column, because a query can select it.

**The backward adjacency is not stored for every link.** It serves joins that start from the parent: a right or full join, a backward traversal, and a semi-join reduction from child to parent. It does not store edge properties. A query that needs a child column reads it through the child row id.

### 13.2.3 The cost of a link on insert

A foreign key check in PostgreSQL probes the parent's unique index for each inserted child row, and it takes a `FOR KEY SHARE` lock on the parent row. The probe returns the parent's row. In rupg it returns the row id. So the forward link value of a new child row is known at no extra cost: the insert writes the parent's row id into the hidden column together with the row. For a link without a foreign key, the insert makes the same probe without the lock.

The cost is the bytes of the hidden column. In the hot store it is 8 bytes for each row before page compression. In the cold store it is bit-packed. On a TPC-C `order_line` row of about 60 bytes, 8 bytes is 13 percent. Document 24 keeps open whether hot rows store the link or compute it at the move (question 1).

### 13.2.4 Links under transactions

**The forward link is a column of the child row.** In the hot store it is part of the row, so an update writes its old value to the undo record like any other column, and a snapshot sees the link value of the version that it sees. In the cold store it is a column of the segment, immutable with the segment. An update of the foreign key of a cold row deletes it in the segment and inserts it in the hot store (document 10 section 10.5), so the new value is in the hot row. The link is therefore transactional with no mechanism of its own.

**A trusted link needs an enforced constraint.** A link created by a foreign key that is valid and enforced is trusted. PostgreSQL's referential rules then guarantee that a visible child row points to a visible parent row with the same key, at every snapshot: `NO ACTION` and `RESTRICT` prevent the parent change, and `CASCADE`, `SET NULL` and `SET DEFAULT` change the child in the same transaction, which changes its link. The executor uses a trusted link with no check.

**Any other link is a validated link.** A link from DDL, a link on a `NOT VALID` constraint, and a link written while referential triggers were off are validated. `session_replication_role = replica`, which `pg_restore` and logical replication use, turns referential triggers off, so a write in that mode turns the link to validated until the next full check of section 13.2.5. The executor checks each gathered pair: the parent row must be visible to the snapshot, and the parent key must equal the child key. A pair that fails the check is resolved by a probe of the parent's unique index. The check compares two values that the join reads anyway, so its cost is one comparison for each row.

**A parent delete or key change does not touch children.** The trusted case cannot have one without a child change. The validated case catches it in the check.

### 13.2.5 Section states and maintenance

Each part has a state in the table's section directory: complete, partial or stale. The optimizer reads the state when it chooses a plan (document 15).

| Event | Forward link | Monotone link | Backward adjacency |
|---|---|---|---|
| child insert | extended by the insert, complete | stays complete only for an append in key order, else dropped to partial | partial: the new rows are not in it |
| child delete | unchanged, the row is invisible | unchanged | unchanged, the executor tests visibility |
| child update of the key | in the row, complete | stale for that row, so partial | partial |
| mover writes a segment | copied into the segment | rebuilt for the segment if both sides are sorted | rebuilt for the new segment |
| parent insert | unchanged | partial | unchanged |

**A partial backward adjacency still works.** It covers the cold child segments up to a generation. The executor uses it for those and reads the forward links of the hot rows and newer segments for the rest. When the uncovered tail passes 5 percent of the child rows, which is rudb's threshold, the maintenance task rebuilds it in queue 3.

**A full check rebuilds trust.** `rupg_link_check(regclass)` recomputes every forward value by a probe of the parent key, compares, repairs, and marks the link trusted if its constraint is enforced. It runs as maintenance and after a restore. The checkpoint writes the parts in rudb's order: columns, key maps, links, adjacency, the section table, the directory, and then the header slot (document 08 section 8.6).

### 13.2.6 The budget

All links of a table must fit in `rupg.graph_budget`, which is a share of the table's compressed column bytes. The default is 0.10, which is rudb's budget. Parachute used 15 percent and reported 1.54x on JOB. When the links of a table would pass the budget, the loader keeps the forward links with the highest estimated benefit first, then the backward adjacencies. The estimate uses the parent's row count and the fan-out from the summaries, not queries. A link that is not built is not an error. `EXPLAIN` then shows a hash join.

## 13.3 Executing over links

Document 14 section 14.8 specifies the join operators. This section lists the uses that a link makes possible, so that document 15 can cost them.

**Link join.** A gather of parent columns by the forward link. An inner join drops the sentinel. A left join gathers null for it. A semi-join tests only that the value is not the sentinel, and an anti-join tests that it is. A right or full join needs the backward adjacency or a hash join.

**Exact semi-join reduction.** A filter on the parent gives a bitmap of parent row ids. The forward link turns it into a bitmap of child rows with no false positives, and the zone maps of the link column skip child vectors that point to no selected parent. This is a full Yannakakis reduction, with no Bloom filter. Its risk is cache misses on a large parent bitmap: 150 million parents are 18.75 MB, which is larger than the L2 cache of one core.

**Backward group by.** A group by on the parent key over child rows becomes a group by on the parent row id, which is a dense array indexed by local number. Q18 of TPC-H groups `lineitem` by order, which needs an array of 150 million entries. The executor uses this only when the array fits the operator's memory reservation.

**Factorized expansion.** A backward traversal produces, for each parent, a run of children. The executor keeps it as an `Expanded` vector (document 14) and computes `count`, `sum`, `min` and `max` over runs without a flatten. The rudb graph notes cite FFX at 2.08x on average and up to 9.39x. Groß, ten Wolde and Boncz (CIDR 2025) measured factorized aggregation at 1.25x, and 17.58x with caching.

## 13.4 The vector index

### 13.4.1 The SQL surface

rupg serves the SQL surface of pgvector, because it is the vector interface that PostgreSQL users already have. `CREATE EXTENSION vector` creates the types `vector`, `halfvec`, `sparsevec` and the operators on `bit`, with pgvector's limits: up to 2,000 dimensions for an indexed `vector`, 4,000 for `halfvec`, 64,000 for `bit`, and 1,000 non-zero elements for `sparsevec`. The distance operators are `<->` (L2), `<#>` (negative inner product), `<=>` (cosine), `<+>` (L1), `<~>` (Hamming) and `<%>` (Jaccard). The operator classes have pgvector's names, such as `vector_l2_ops` and `vector_cosine_ops`.

| Access method | rupg layout | Parameters mapped |
|---|---|---|
| `hnsw` | proximity graph | `m` sets the out-degree to 2m, `ef_construction` sets the build beam |
| `ivfflat` | partitioned | `lists` sets the partitions |
| `vector` (rupg name) | chosen at build from the row count | `layout`, `degree`, `beam` |

The settings `hnsw.ef_search`, `ivfflat.probes`, `hnsw.iterative_scan`, `hnsw.max_scan_tuples` and `ivfflat.iterative_scan` keep pgvector's names, defaults and meaning. A query is `ORDER BY embedding <-> $1 LIMIT 10`, as in pgvector, and the plan shows an `Index Scan` on the index.

**rupg does not run pgvector's code.** pgvector is a C extension, and document 16 excludes C extensions. rupg implements the surface in `rupg-contrib` and the index in `rupg-ann`. The result of an approximate search can differ from pgvector's result on the same data, as pgvector's result can differ between two builds of the same index. The tests of section 13.8 compare recall, not rows.

### 13.4.2 The graph layout

The graph layout is a single-layer proximity graph built by the Vamana method of DiskANN, not the multi-layer graph of HNSW. A single layer has one entry point and one neighbour list for each node, so a node fits in one place on a page, and a search reads pages in a pattern that the buffer manager can prefetch. `USING hnsw` maps to this layout because the parameters translate: HNSW's bottom layer has an out-degree of 2m, and its `ef_search` is a search beam.

**The graph lives in the file's own pages.** Each node is a record in 16 KiB index pages of the buffer manager: the row id, the neighbour list as local row numbers, and the quantized code of section 13.4.3. The full vectors stay in the table, in the hot row or the cold segment. This is the design of DiskANN in Azure Cosmos DB (PVLDB 18(12), 2025), which keeps the index in the database's own Bw-tree and reported a query latency under 20 ms at 10 million vectors. One file then holds the data and the index, with one checkpoint, one recovery and one backup.

**The partitioned layout** is for `ivfflat` and for small tables. It clusters the vectors into lists with k-means, stores the quantized codes of each list together, and searches the nearest `probes` lists. Lu et al. (SIGMOD 2026, arXiv 2603.23710) found that inside PostgreSQL a clustering method like ScaNN can beat graph methods. So rupg keeps both layouts and does not assume that the graph wins. The benchmark of section 13.8 decides the default for `USING vector`.

### 13.4.3 Quantization

Each node stores a RaBitQ code (Gao and Long, SIGMOD 2024): one bit for each dimension, with a distance estimate that has a proven error bound. A search ranks candidates by the estimate. A candidate whose bound overlaps the current k-th distance is ranked again with the full vector from the table. Others are dropped with no read of the full vector. A 1536-dimension code is 192 bytes, against 6,144 bytes for the `float4` vector.

Extended RaBitQ (Gao et al., SIGMOD 2025) uses more bits for each dimension and gives a better estimate, but its encoding is slow: Weaviate reported 4,000 µs or more for one 1536-dimension vector at 8 bits. An insert path that must stay near the point path budget cannot pay that. **Decision: inserts write base RaBitQ codes.** A maintenance task can rewrite codes in the extended form in queue 3, if the benchmark of section 13.8 shows a recall gain at the same latency.

### 13.4.4 Filtered search

A real query has a filter: `WHERE tenant_id = $2 ORDER BY embedding <-> $1 LIMIT 10`. A graph search that drops nodes that fail the filter loses recall when few nodes pass. rupg chooses one of three strategies from the estimated selectivity s of the filter (document 15).

| Strategy | When | How |
|---|---|---|
| pre-filter scan | the filter passes fewer than `rupg.vector_prefilter_rows` rows (default 50,000) | evaluate the filter, then rank the passing rows by their RaBitQ codes and rerank, which is exact |
| filtered traversal | s between 1 percent and 50 percent | traverse the graph and expand through failed nodes to their neighbours, in the manner of ACORN-1 |
| post-filter | s above 50 percent | normal search with the iterative scan of pgvector 0.8, until k rows pass or `max_scan_tuples` is reached |

The thresholds are budgets until the benchmark sets them. ACORN (Patel et al., SIGMOD 2024) reported 2x to 1,000x more queries per second than earlier filtered methods. Its full form builds 8.8x to 33.1x slower than HNSW, so rupg uses only the traversal rule of ACORN-1, which needs no special build. ACORN-1 recall falls to 0.45 to 0.72 at selectivities of 0.3 percent to 1 percent, which is why the pre-filter scan takes over below 1 percent. SeRF and iRangeGraph, which build for range filters, do not support updates, so rupg does not use them.

### 13.4.5 Updates

**An entry is a row id, as in section 12.4.** A search reads the row version that its snapshot sees, and drops a row whose vector changed since the node was written. An update of a vector inserts a new node and marks the old node deleted. A delete marks the node. A marked node stays in the graph as a waypoint, so the graph stays connected. A maintenance task consolidates marked nodes in the manner of FreshDiskANN (Singh et al., 2021): it rewires their neighbours to skip them and frees them. It runs when marked nodes pass 10 percent of the graph, which is a budget.

A bulk load builds the graph after the segments are written, in parallel, with the full vectors in memory up to the operator memory budget, and in batches above it. The build time counts in the load time of document 20 section 20.3.

### 13.4.6 Targets

The published reference for a PostgreSQL vector index at scale is a vendor number: pgvectorscale's StreamingDiskANN reported 28 ms p95 against 784 ms for Pinecone and 16x more queries per second, at 50 million 768-dimension vectors and 99 percent recall. It is a vendor benchmark, and it compares with a service, not with PostgreSQL. rupg does not use it as a gate.

The rupg target is a budget, measured by the harness of document 20: at the same recall at 10, rupg must serve 10x the queries per second for each core of pgvector 0.8 with HNSW on PostgreSQL 19, on the same machine, with half the memory. Document 24 keeps open whether this becomes a gate (question 4).

## 13.5 Graph queries

### 13.5.1 Recursive queries over links

`WITH RECURSIVE` is PostgreSQL syntax, and rupg runs it with PostgreSQL's semantics, including `UNION` duplicate removal, `UNION ALL`, and the `SEARCH` and `CYCLE` clauses of PostgreSQL 14. When the recursive term joins the working table to a table through a link, as in a walk of `employee.manager_id`, the optimizer can run the step as a traversal of the backward adjacency with a visited bitmap over row ids, instead of a hash join for each iteration. The output order, the cycle marks and the search columns must be the same as in PostgreSQL. A query whose semantics the traversal cannot keep, such as one with a `LIMIT` that depends on the iteration order, uses the general plan.

### 13.5.2 Property graph syntax

SQL:2023 Part 16, SQL/PGQ, defines `CREATE PROPERTY GRAPH` and the `GRAPH_TABLE` operator with a `MATCH` pattern. PostgreSQL 19 development added SQL/PGQ and then removed it in beta 4, so PostgreSQL 19 has none of it. A PostgreSQL client therefore never sends it.

**Decision: rupg accepts property graph syntax only when `rupg.property_graph` is on.** The default is off. When it is off, the statements fail with SQLSTATE `42601`, the syntax error that PostgreSQL 19 gives. When it is on:

1. `CREATE PROPERTY GRAPH` names vertex tables and edge tables. Each edge table must have two links, one to its source vertex table and one to its destination.
2. `GRAPH_TABLE (g MATCH (a)-[e]->(b) COLUMNS (...))` is rewritten by the analyzer to joins over those links, so the optimizer and the executor see ordinary joins.
3. Path patterns with a quantifier, and `ANY SHORTEST`, run as a breadth-first search over the backward adjacency. Many sources run together as a multi-source BFS with one bit for each source in each visited word, the method of DuckPGQ (CIDR 2023, PVLDB 16 demo). A thesis on DuckPGQ reported up to 11.46x on shortest paths at LDBC SNB SF300.

The property graph catalog lives in the rupg catalog. It has no rows in `pg_catalog`, because PostgreSQL 19 has no tables for it. `pg_dump` from PostgreSQL does not know it, so `rupg-cli` provides a dump option that writes the graph definitions after the tables. Document 24 keeps open whether rupg should follow the syntax that a later PostgreSQL release adopts, if it differs from SQL:2023 (question 5).

**No graph engine is a dependency.** Kuzu was the embeddable property graph system closest to this design. Apple agreed to acquire it on 9 October 2025, and its repository was archived on 10 October 2025 at version 0.11.3. rupg uses the CSR and BFS code lifted from `rudb-graph` only.

## 13.6 The honesty rule and the vector index

Links and adjacencies meet the five rules of document 02. They are built from the schema and the data. Their build time and bytes count. Deleting them changes no answer, because the hash join gives the same rows. They are transactional by section 13.2.4.

**The vector index is the exception to rule 4.** An approximate search with the index can return different rows from an exact sort without it. This is PostgreSQL's behaviour with pgvector, and users choose it when they create the index. So the delete test of document 21 section 21.8 does not cover vector indexes. Instead, the vector tests check recall against an exact answer computed with `enable_indexscan = off`, and they check that every returned row satisfies the filter of the query. A returned row that fails the filter is a bug, not a recall loss.

## 13.7 Shards and graph indexes

**A link is inside one shard.** A child row and its parent must be in the same shard. This holds when the two tables are sharded on the same key, as `orders` and `lineitem` on the order key, or when the parent is a replicated table that every shard holds. For any other foreign key in a cluster, `rupg.auto_link` creates no link, and the join is a hash join with an exchange (document 18). `CREATE INDEX ... USING link` on such a pair fails with SQLSTATE `0A000`.

**A vector index is local to each shard.** A query asks each shard for its k nearest rows and merges the lists. The merge is exact over the shard answers, so the recall of the cluster is at least the lowest recall of a shard for the same k.

**A graph traversal that crosses shards** sends frontier row ids to the shard that owns them, one batch for each BFS level. Document 18 owns the exchange. This is a M12 feature.

## 13.8 Milestones and tests

| Milestone | Work |
|---|---|
| M6 | links from foreign keys and DDL, forward and monotone forms, backward adjacency, trusted and validated modes, the link join and exact reduction, `rupg_link_check`, the vector index with both layouts, RaBitQ, filtered search, the pgvector surface |
| M8 | link budgets measured on JOB and TPC-H SF100, the factorized operators, the default layout of `USING vector` |
| M11 | co-location rules, sharded vector search |
| M12 | property graph syntax, cross-shard traversal |

**Link tests.** Every JOB and TPC-H query runs with links and with `rupg.use_links = off`, and the answers must match. A fuzz test changes foreign keys, deletes and reinserts parents, writes with `session_replication_role = replica`, and checks each join against a hash join at random snapshots. The crash tests of document 21 kill the process during a link rebuild and check the section states after recovery.

**Vector tests.** The ANN-Benchmarks data sets SIFT-1M and GloVe, and a 1536-dimension set of 10 million vectors, run with pgvector 0.8 on PostgreSQL 19 and with rupg on the same machine. The report gives recall at 10, queries per second for each core, p99 latency, build time and memory for both. The filtered tests use three filter selectivities: 0.5 percent, 5 percent and 50 percent.

## 13.9 Open questions

1. Whether hot rows store the forward link, which costs 8 bytes for each row, or the mover computes it when it writes the segment, which makes hot joins use a hash join (section 13.2.3).
2. Whether the loader may infer links from data on a schema without foreign keys, when a column's values are contained in a unique column of another table. The honesty rule allows a rule from data, but the number of candidate pairs needs a limit (section 13.2.1).
3. Whether the filtered search thresholds of section 13.4.4 hold across data sets, or need a cost model of their own.
4. Whether the vector target of section 13.4.6 becomes a gate, and at which milestone.
5. Whether rupg follows the property graph syntax of a later PostgreSQL release if it differs from SQL:2023 (section 13.5.2).
