# 12. Indexes

Written 7 October 2026. PostgreSQL facts are from the PostgreSQL 18 documentation and the PostgreSQL 19 beta 4 source tree. rudb facts are from the rudb notes named in each section.

This document specifies the index layer of rupg. It covers the six PostgreSQL index families, the way one index covers rows in the hot store and the cold store, index entries under in-place MVCC, the summaries that act as access paths without any DDL, the certified frequency synopses of H3 and the gram summaries of H4. Document 13 specifies the link and vector indexes. Document 15 specifies how the optimizer chooses among the access paths that this document defines. The crate is `rupg-index` at rank 6, with the summaries in `rupg-summary` at rank 4 and the generic B+tree in `rupg-tree` at rank 2.

## 12.1 What an index is in rupg

An index in rupg has two jobs. The first job is the PostgreSQL job: it finds the rows that match a key, it enforces a unique or exclusion constraint, and it gives rows in an order. The second job is the analytic job: it tells a scan which parts of the table it can skip, and for some aggregates it tells the executor the answer for a whole segment without a read of the values. PostgreSQL does the second job only with BRIN. rupg does it with every column, by default, through the summaries.

The two jobs have different costs. A PostgreSQL index has one entry for each row, so on 100 million rows it costs gigabytes and it costs time on every insert. A summary has one entry for each vector of 1024 values or each segment of up to 262,144 rows, and the mover builds it when it writes the segment. So rupg keeps the first job in the index families that users create, and puts the second job in structures that every table has.

**An index is never required for a correct answer.** If the optimizer ignores an index or a summary, the plan scans the table and the answer is the same. Unique, primary key and exclusion constraints are the exception, because the index checks the constraint. Section 12.4 gives the rules.

## 12.2 The access method interface

Clients see access methods through `pg_am`, `pg_opclass`, `pg_opfamily`, `pg_amop` and `pg_amproc`, and `psql`, `pg_dump` and ORMs read them. rupg must show the same rows with the same OIDs as PostgreSQL 19, built by the virtual catalog of document 07.

| Name in `pg_am` | OID | Type | rupg implementation | Milestone |
|---|---|---|---|---|
| `heap` | 2 | table | the hot and cold stores of document 10 | M1 |
| `btree` | 403 | index | dense and sparse forms, section 12.5.1 | M3 for primary keys, M6 in full |
| `hash` | 405 | index | linear hash over row pages, section 12.5.2 | M6 |
| `gist` | 783 | index | GiST tree over row pages, section 12.5.4 | M6 |
| `gin` | 2742 | index | inverted index with posting lists, section 12.5.3 | M6 |
| `spgist` | 4000 | index | space partitioned tree, section 12.5.5 | M6 |
| `brin` | 3580 | index | the zone maps of section 12.6 | M6 |
| `link` | rupg OID | index | document 13 | M6 |
| `vector` | rupg OID | index | document 13 | M6 |

`hnsw` and `ivfflat` are the access methods of the pgvector extension, and they exist only after `CREATE EXTENSION vector`. rupg keeps that rule: the extension script in `rupg-contrib` creates the two `pg_am` rows and maps both to the vector family of document 13. `link` and `vector` are rupg names with OIDs from the rupg range of the pin (document 07). They appear in `pg_am` only when `rupg.show_extension_am` is on. The default is off, so a tool that lists access methods sees what it sees on PostgreSQL.

**The Rust interface is one trait.** `rupg-index` defines `IndexAm`. It carries the capability flags of PostgreSQL's `IndexAmRoutine`, because `pg_indexam_has_property` and its two sibling functions expose them. Each family must report the same values as its PostgreSQL namesake, and the suite of document 05 section 5.6 compares them against PostgreSQL 19.

A scan returns batches of up to 1024 row ids, and for an index-only scan the key columns as a vector. So the executor of document 14 uses the same operators above an index scan as above a table scan. The point path of document 14 section 14.9 uses a single-key call, `probe_one`, that returns one row id with no allocation.

**Operator classes keep their PostgreSQL meaning.** rupg ships every operator class of the core families in PostgreSQL 19, with the same names, strategies and support numbers. A support function is a rupg function, not a C function. `CREATE OPERATOR CLASS` works for a user type when every support function is SQL or PL/pgSQL, because document 16 excludes C extensions.

## 12.3 Hot rows and cold rows in one index

The design question is this: a table has rows in the hot store, which is a B+tree of 16 KiB row pages, and rows in the cold store, which is immutable compressed segments. The mover moves rows from the first to the second. How does an index cover both?

**Decision: one index covers both stores, and its entries point to row ids.** A row keeps its row id across updates and across the move to the cold store. The location of a row id is found through the hot store's tree or through the row id to segment map of document 10 section 10.2. So an index entry does not change when the mover moves a row. The mover does not touch dense indexes. A unique check and a point lookup probe one structure. An index scan does not need a merge of a hot part and a cold part.

**Rejected: an index for each segment.** ClickHouse and DuckDB attach skip indexes to each part or row group. With a mover that writes segments over time, key ranges overlap, so a probe and a unique check visit every overlapping segment and the cost grows with table age. A TPC-C New-Order transaction with the average of 10 order lines makes about 25 to 35 index probes by the transaction profile of the TPC-C specification, and the CPU budget of document 02 does not allow that growth.

**Rejected: an index on the hot store only.** Q19 of ClickBench (`WHERE UserID = 435090932899640449`) would then scan 100 million values, and the C4 budget is 1 ms.

### 12.3.1 The dense form

The dense form is a B+tree of `rupg-tree` with 16 KiB pages. Each leaf entry holds the key columns, the `INCLUDE` columns, the row id and one state byte. It is the only form of hash, GiST, GIN and SP-GiST indexes.

The dense form is expensive on large analytic tables. ClickBench's `hits` table in the PostgreSQL variant declares `PRIMARY KEY (CounterID, EventDate, UserID, EventTime, WatchID)`, a key of 4 + 4 + 8 + 8 + 8 = 32 bytes. With the row id and the state byte, 100 million entries are about 4.1 GB before prefix compression, plus a sorted build in the load. The disk and load targets of document 02 do not allow it.

### 12.3.2 The sparse form

**Decision: a B-tree index whose key columns are a prefix of a segment sort order uses the sparse form for cold rows.** The sort order and the projections are chosen at load (document 10 section 10.7). When the rows of a segment are in the order of the index key, the segment itself is the leaf level of the index. The sparse part then stores one fence entry for each vector: the first key of the vector, the segment and the vector number. 100 million rows give 97,657 fence entries. At 40 bytes each that is 3.9 MB, which is about one thousandth of the dense form.

A probe finds the fence, decodes one vector of the key columns and searches it. A bit-packed vector of 1024 values decodes in about 1 µs with the rudb kernels (a prediction from kernel throughput, not a rupg measurement), which is about the cost of one buffer pool miss on a dense probe.

A mover segment is sorted inside itself, but its key range can overlap other segments, so a probe visits each overlapping segment through an interval tree of segment key ranges. The maintenance task of document 10 merges overlapping segments when a probe visits more than four segments on average. Four is a budget, and the merge counts as maintenance work in document 20.

The hot rows and the rows of unsorted segments stay in a dense part. When the mover writes a sorted segment, it deletes the dense entries of the moved rows in the same transaction. This is the only case where the mover touches an index. A projection of document 10 section 10.7 that holds rows in another order has the row identity of each row, so it can serve as the sparse part of a second index at almost no extra cost.

A projection of document 10 section 10.7 that holds rows in another order can serve as the sparse part of a second index. The projection has the row identity of each row, so a probe gets the row id from it. This makes a projection a second sorted access path at almost no extra cost.

**The user does not choose the form.** The index layer picks it for each segment. `pg_relation_size` reports the bytes of both parts, and `EXPLAIN` shows the same `Index Scan` node for both, with `Cold Part: sparse` in the verbose form (document 15 section 15.9).

### 12.3.3 The tree

`rupg-tree` holds the inner nodes of each tree in rupg. The hot store, the catalog, TOAST and the dense form give their own leaf format through the `Leaf` trait. The tree reaches the pages only through `PageAccess` (spec/22 section 22.4).

**The inner node.** An inner node is a 16 KiB page with the page header of spec/08 section 8.3.1 and the inner kind of its user, for example kind 5 for the hot store. The kind data holds the level at bytes 0 and 1, the count of keys at bytes 2 and 3, and the leftmost child at bytes 8 to 15. Leaves are level 0. A slot array of `u16` offsets follows the header in key order. Each entry is at the end of the page: the child as a `u64`, the key length as a `u16`, then the key. Keys compare as bytes. The leftmost child holds the keys below the first key, and the child of entry `i` holds the keys at or above key `i` and below key `i + 1`. A key is at most 2,704 bytes, the B-tree key limit of PostgreSQL (section 12.13 question 1), so six keys fit in one node.

**Latches.** A reader or a writer goes down with optimistic reads and latches only the leaf. After it takes the latch, it checks that the parent did not change, and it starts again from the root if the parent changed. A writer whose leaf is full goes down again with exclusive latches. It releases the latches above an inner node that has room for one more key. Each split holds the latch on the parent, so the parent check finds each split that a reader can miss. This is the optimistic lock coupling of Leis, Haubenschild and Neumann (IEEE Data Eng. Bull. 42, 2019).

**Splits.** A full inner node splits at the middle of its bytes and not at the middle of its count, so each half fits when the keys have different lengths. The middle key goes up to the parent. The root page does not move. A root split copies the root to a new page, splits that page, and writes the root again one level up, so a catalog entry can keep the root page number.

**No merges at M1.** A separator stays in the tree after it goes in. A scan uses this rule: it goes down again from the root at the upper bound of each leaf, so the leaves need no sibling links. Merges come with purge at M3.

## 12.4 Index entries and MVCC

rupg updates rows in place with undo records, so an index entry points to a row, not to a tuple version. The rules follow InnoDB's secondary index design, which has the same problem, adapted to commit timestamps.

**An entry is a key and a row id.** When an update changes a key column, the transaction inserts a new entry and sets the delete mark on the old one, and both changes go to its undo buffer. An update that does not change a key column of an index does not touch that index. This is the in-place equivalent of PostgreSQL's HOT updates.

**A reader checks the row, not the entry.** An index scan reads the row version that its snapshot sees and evaluates the index condition again on it. An entry whose key does not match is dropped. A marked entry can still match a version that an old snapshot sees, so a mark alone does not drop an entry.

**Each leaf page has a change timestamp.** The page header holds the highest commit timestamp of any change to the page and the count of uncommitted changes on it. An index-only scan with snapshot S can use the entries of a page without a visit to the rows if the change timestamp is at or below S and the uncommitted count is zero. Then every unmarked entry is visible and every marked entry is invisible. Otherwise the scan visits the row for each entry of the page. This replaces PostgreSQL's visibility map, and `EXPLAIN ANALYZE` reports the visits as `Heap Fetches` with the same meaning.

**Purge removes marked entries** when no snapshot is older than the change that marked them. It runs in queue 3 of the scheduler, and `VACUUM` runs it at once for a table.

**A unique check waits as PostgreSQL waits.** An insert reads the row of each entry with the same key. A committed visible match fails with SQLSTATE `23505` and PostgreSQL's detail message. A match from an uncommitted transaction makes the insert wait for it and check again. `NULLS NOT DISTINCT` and deferrable constraints follow PostgreSQL's rules.

**Predicate locks go on key ranges.** A scan in a `SERIALIZABLE` transaction records the key range that it read, at leaf page granularity, as PostgreSQL does. Document 11 owns the conflict checks.

**The key size limit is PostgreSQL's limit.** PostgreSQL refuses a B-tree entry above 2,704 bytes with SQLSTATE `54000`. 16 KiB pages allow more, but rupg keeps 2,704 bytes, because the regression suite tests the error and an application must still work when it moves back to PostgreSQL. Document 24 keeps this open (question 1).

## 12.5 The PostgreSQL index families

### 12.5.1 B-tree

rupg must support every B-tree feature of PostgreSQL 19: multicolumn keys, `ASC` and `DESC`, null ordering, `INCLUDE` columns, partial and expression indexes, unique and primary keys, `NULLS NOT DISTINCT`, deduplication, backward scans, `x = ANY($1)` searches, and the skip scan of PostgreSQL 18.

**Skip scan is free in the sparse form.** PostgreSQL 18 can serve `WHERE b = 5` from an index on `(a, b)` when `a` has few distinct values. In rupg's sparse form the fences hold the first key of each vector, so a scan on a non-leading column reads only the vectors that the fences allow. The ClickBench key starts with `CounterID` and `EventDate`, and the C9 queries filter on `CounterID = 62` and a date range, so they read a few vectors of a few segments. This is how C9 meets its 5 ms budget.

**Deduplication is the leaf format.** Each leaf stores each distinct key once with a delta-coded list of row ids, and the common prefix of its keys once. Internal pages store the shortest separator, as the suffix truncation of PostgreSQL 12 does. The `deduplicate_items` parameter is accepted and shown by `\d+`.

**Prefix compression applies to the key columns.** Each leaf stores the common prefix of its keys once. Each internal page stores the shortest separator that divides its children, as in the suffix truncation of PostgreSQL 12.

**`CLUSTER` sets the sort key.** `CLUSTER t USING idx` records the index key as the sort key for new segments and rewrites the cold segments in that order as a maintenance task.

### 12.5.2 Hash

A hash index supports only equality on one column and cannot be unique, and rupg keeps these limits because the property functions expose them. It is a linear hash table of 16 KiB bucket pages over the 32-bit hash of the key, with overflow pages, as in PostgreSQL. The optimizer treats it as a B-tree that serves equality only.

### 12.5.3 GIN

GIN indexes a composite value by its parts: array elements, `jsonb` keys and values, `tsvector` lexemes or `pg_trgm` trigrams. rupg stores each posting list as a Roaring bitmap over the low 48 bits of the row id, so an intersection is a bitmap AND and the result is a row id filter for a scan.

**The pending list follows PostgreSQL.** With `fastupdate` on, new entries go to an unsorted pending list that is merged when it passes `gin_pending_list_limit` (4 MB by default) or at `VACUUM`. rupg keeps this, because `gin_clean_pending_list` and the statistics views expose it.

**Builds are parallel**, as PostgreSQL 18 made GIN builds. rupg builds every family in parallel: workers extract and sort parts from ranges of segments, and the build merges the runs. `max_parallel_maintenance_workers` caps the workers.

### 12.5.4 GiST

GiST is a balanced tree over a user-defined key such as a box, a range or a signature. rupg builds it over 16 KiB pages with PostgreSQL's support functions, and uses the sorted build of PostgreSQL 14 when the operator class has `sortsupport`. It serves overlap and containment, `ORDER BY p <-> point '(0,0)'` and exclusion constraints.

**Exclusion constraints and `WITHOUT OVERLAPS` use GiST.** The temporal keys of PostgreSQL 18 need a GiST index and fail with SQLSTATE `23P01` on a conflict. Their scalar parts need `btree_gist`, which ships in `rupg-contrib` (document 16).

### 12.5.5 SP-GiST

SP-GiST supports trees that split space into parts that do not overlap: quad trees, k-d trees and radix trees. rupg implements its five support functions and the shipped operator classes for points, boxes, ranges, `inet` and `text`, with nearest neighbour ordering. It has no role in the performance gates, and its tests are PostgreSQL's regression files.

### 12.5.6 BRIN

**Decision: BRIN is the zone maps.** A PostgreSQL BRIN index stores a summary for each range of `pages_per_range` heap pages (128 by default). rupg already stores a minimum and a maximum for each vector of the cold store (section 12.6), so `USING brin` with the `minmax` class adds no structure for cold rows. For hot rows it builds a summary for each run of `pages_per_range` row pages. For the `bloom` and `minmax_multi` classes it builds the summary for each cold vector, and the bytes count in the index size.

`brin_summarize_new_values` and its sibling functions act on the hot part. On the cold part they return 0, which PostgreSQL returns when every range is summarized.

## 12.6 Summaries as access paths

Every cold column has summaries. Users do not create or drop them. `rupg.use_summaries` (default on) turns off their use, for tests. Document 09 section 9.7 specifies the segment contents: row count, null count, minimum, maximum, sum and the set of dictionary codes present, plus a minimum and a maximum for each vector.

### 12.6.1 Three uses

**Pruning.** A filter is tested against each segment summary and then each vector summary, with three results: no row can pass, every row passes, or some rows can pass. The scan skips the first case, does not evaluate the filter in the second, and reads the vector in the third. Ranges use minimum and maximum, equality and `IN` on codes use the code set, and null tests use the null count.

**Answering.** An aggregate over a segment that passes in full comes from the summary: `count(*)` from the row count, `count(x)` from the null count, `sum` and `avg` from the sum, `min` and `max` from the bounds, and `count(DISTINCT x)` on a coded column from the union of code sets. This is H1 of document 02, for C1 and C8.

**Bounding.** Document 15 uses the summaries for cardinality estimates, and document 14 uses the key range to size a dense group by array.

### 12.6.2 Summaries under MVCC

A delete of a cold row sets a bit in the segment's delete mask with the commit timestamp of the delete, and an update of a cold row is a delete there plus an insert in the hot store under the same row id (document 10 section 10.5). So a summary describes the rows that the segment held when it was written.

**Rule: a summary answers for a snapshot only after a correction for the deletes visible to that snapshot.** For a snapshot S, let D be the rows of the segment whose delete committed at or before S. If D is empty, the summary is exact for S. If D is not empty:

| Summary | Use when D is not empty |
|---|---|
| row count, null count | exact after the subtraction of the counts of D, read from the delete mask and the null bitmap |
| sum | exact after the subtraction of the values of D, read from the column at the positions of D |
| minimum, maximum | a bound, valid for pruning, not for an answer |
| code set | a superset, valid for pruning, not for `count(DISTINCT)` |

When a summary is only a bound, the executor reads the segment for that aggregate. D is usually small, and the mover rewrites a segment when D passes a share that document 10 sets. Document 11 owns the visibility rule for segments and deletes.

**The hot store has no summaries.** Every plan scans the hot rows, which are few by design, and combines them with the summary answer for the cold store. This is why summary answers stay correct under writes, which is rule 5 of the honesty rule in document 02.

### 12.6.3 What the planner sees

The planner treats the summaries as one implicit access method that every table has. It is not in `pg_index` or `pg_am`. `EXPLAIN` shows it as properties of the `Seq Scan` node, so a tool that parses plans still finds a `Seq Scan` (document 15 section 15.9).

**Floating point sums can differ in the last bit.** rupg stores a `float8` sum as a compensated sum, so a summary answer can differ in the last bit from a scan, as two PostgreSQL plans can. The regression suite compares floating point aggregates with PostgreSQL's tolerance rules. Integer and `numeric` sums are exact in 128 bits.

## 12.7 Building and maintaining indexes

**`CREATE INDEX` is a parallel bulk build.** The build reads the table at a snapshot, sorts the keys and row ids in runs of `maintenance_work_mem`, merges them and writes the tree bottom up. The normal insert path is active from the start, so later commits are added. This is the method of `CONCURRENTLY`, and rupg uses it for both forms. Plain `CREATE INDEX` still takes a `SHARE` lock and blocks writes, as in PostgreSQL.

**Bulk load builds indexes in bulk.** `COPY` and large `INSERT ... SELECT` write segments directly (document 10 section 10.6), sort the new keys, and merge them into each dense index as one batch at commit, which writes each leaf once. The time counts in the load time of document 20 section 20.3.

**`REINDEX` builds a new index and swaps it** in one catalog transaction, as `REINDEX CONCURRENTLY` does in PostgreSQL 12 and later.

**`VACUUM` purges.** It removes marked entries, merges the GIN pending list, summarizes new BRIN ranges and merges overlapping sorted segments. It reclaims no row versions, because rupg has no dead tuples (document 11).

**Every family has a check.** `check` verifies key order, separators, and a one-to-one match between entries and visible rows. `bt_index_check`, `bt_index_parent_check` and the other `amcheck` functions of PostgreSQL 19 call it, and the crash tests of document 21 run it after every recovery.

## 12.8 Certified frequency synopses

A C3 query groups by a key with many distinct values, orders by count and keeps the top 10, as Q15 does with `UserID`. At 1 million rows rudb measured 898,913 distinct values of `UserID` (`../2140/storage-v3/11-certified-frequency-synopses.md`). A hash aggregate of that shape over 100 million rows has a table that does not fit in cache, and document 02 allows about 20 cycles per row. H3 is the answer. The design is lifted from that rudb note into `rupg-summary`.

### 12.8.1 What a synopsis is

A synopsis for a column is a list of up to 512 values with exact counts, and `omitted_max`, an upper bound on the count of every value not in the list. One rule builds it for every column that qualifies, and the rule does not look at queries.

**The numeric build** follows rudb: a Misra and Gries pass with 32,768 entries that counts its decrement rounds, a second pass for exact counts when there were decrements, the 512 largest kept, and `omitted_max` set to the larger of the decrement count and the first exact count cut. **The string build** counts each global dictionary code exactly, because the codes are dense integers (document 09 section 9.5). Null is counted apart from the empty string.

**Which columns get one.** An integer, date or dictionary-coded string column whose distinct count is above 1,024. Below that, a dense array group by is cheaper than any synopsis.

### 12.8.2 Where synopses are stored

**Each segment has one** for each qualifying column, built by the mover as part of the segment summary.

**Each table has one for each qualifying column.** A merge of segment synopses is loose, because a value missing from a list is bounded by that segment's `omitted_max`, summed over 381 segments at 100 million rows. So a maintenance task recounts over all cold segments with the same method, records the segment generation it covers, and runs again when the cold rows grow by more than 10 percent, which is a budget. A `COPY` load builds it at the end, inside the load time.

**The bound for a snapshot.** For a value v that is not in the table list, the upper bound on its count at snapshot S is the table `omitted_max`, plus the `omitted_max` of each segment newer than the table synopsis, plus the exact count of v among the hot rows that S sees. The executor computes the last term at query time. Deletes only lower counts, and an update of a cold key is a delete plus a hot insert (document 10 section 10.5), so the bound holds under writes without a transactional update of the list. This is how the synopsis meets rule 5 of the honesty rule.

### 12.8.3 The certificate

Let k be the limit. Let C be the candidate set: the values in the table list, the values in the lists of newer segments, and every value in the hot rows. Let B be the largest bound of section 12.8.2 over the values not in C. The executor counts every value of C exactly at query time. Let `c_k` be the k-th largest of those counts.

**Rule: if `c_k` is strictly greater than B, the top k of C is the top k of the table.** No value outside C can reach `c_k`, because its count is at most B. The comparison is strict, because a value outside C with a count equal to `c_k` could win a tie on a secondary sort key. When the rule fails, the executor runs the full aggregate. The answer is the same in both cases.

**Filters keep the certificate valid.** A filter can only lower counts, so B still bounds the filtered counts. The executor counts the candidates with the filter. The certificate fails more often, because `c_k` falls and B does not.

**Composite keys keep it valid.** The count of a pair `(a, b)` is at most the count of `a`, so the synopsis of `a` bounds every pair whose `a` is not in C. The executor groups the rows whose `a` is in C by the pair and applies the same rule. A key that is an injective function of one column, such as `ClientIP - 1` in Q35, uses that column's synopsis after the optimizer proves the dependency (document 15).

### 12.8.4 The synopsis never supplies the answer

`../2140/storage-v3/34-no-query-answer-blocks.md` states that a grouped query must read row values and compute its groups and counts when it runs, and that a stored frequency list must not be used as grouped output. H3 in document 02 follows the same rule. rupg therefore has one mode, which this document calls count mode.

**Decision: count mode is the only mode.** The synopsis gives the candidate set and the bound, and the executor counts the candidates from row values at query time. The synopsis narrows the rows read and proves the rest cannot change the answer, but never supplies a count. `storage-v3/34` allows this for an index.

Count mode costs one pass over the key column with a membership test. 100 million rows over 16 vCPUs is 6.25 million rows each, so 70 ms allows 11.2 ns for each row. A membership test of a 64-bit key against at most a few thousand candidates in an L1-resident hash set costs about 1 to 2 ns (a prediction from the kernel costs in rudb, not a measurement). When a segment synopsis holds the row ordinals of its candidates, which rudb stores when they are at most 65,536 rows, count mode reads those rows only.

**There is no read mode.** A mode that emits the stored counts directly would put a query answer in the file, which section 2.4.5 of document 02 forbids. The setting `rupg.synopsis_mode` takes `count` (the default) or `off`. `off` makes the planner ignore the synopses, and section 21.8 of document 21 uses it.

### 12.8.5 What it covers

Of the ten C3 queries, Q15, Q33 and Q34 group by one column, Q35 groups by functions of one column, and Q16, Q18, Q30, Q31 and Q32 group by a composite key that has at least one column with a synopsis. The rule of section 12.8.3 works with any column of the key, not only the first. Q17 has a limit without an order, so any 10 groups are correct. Whether the certificate holds depends on the data. The first M8 measurement runs each C3 query in count mode and reports whether it held. A query whose certificate fails needs another mechanism and a new question in document 24.

## 12.9 Gram summaries for LIKE

Class C5 is substring search: Q20 is `SELECT count(*) FROM hits WHERE URL LIKE '%google%'`, and Q21 to Q23 add groups, a second pattern and a sort. The budget is 60 ms each. A scan that tests the pattern on 100 million strings does not fit section 2.4.2 of document 02. H4 is the answer.

### 12.9.1 Two structures

**The dictionary path.** A string column with a global dictionary (document 09 section 9.5) stores each distinct string once. The executor tests the pattern on the dictionary, which gives the set M of codes whose strings match. It then scans the code column and tests each code against M with one bit test. The test on the dictionary costs the bytes of the distinct strings, not the bytes of the rows. This path needs no extra bytes and is always available.

**The dictionary gram index.** When the dictionary is large, a gram index maps each 3-gram of bytes to a Roaring bitmap of the codes whose strings contain it. For `LIKE '%google%'` the executor intersects the bitmaps of `goo`, `oog`, `ogl` and `gle` and tests the pattern only on those codes, which removes false positives.

**The vector gram filter.** A string column with no dictionary stores for each vector a Bloom filter of its 3-grams, sized for a 1 percent false positive rate, which is about 9.6 bits for each distinct gram. A scan skips a vector when a gram of the pattern is absent. The mover writes no filter for a vector whose filter would pass 10 percent of the vector's compressed bytes.

### 12.9.2 Rules

**Grams are bytes after ASCII case folding.** One structure then serves `LIKE` and `ILIKE`. For `LIKE`, the verify step tests the exact case, so case folding only adds candidates. For `ILIKE` with a pattern that has bytes above 127, the gram path is not used, because the case rules of the collation are not ASCII rules.

**The gram path applies to deterministic collations only.** A column with a nondeterministic collation compares strings by rules that grams do not follow. The executor tests the strings in that case.

**Patterns without a gram get no pruning.** A pattern with fewer than three literal bytes between wildcards, such as `'%ab%'`, has no 3-gram. The executor uses the dictionary path without the gram index.

**Regular expressions use required grams.** For `~` and `regexp_replace`, the planner extracts the literal strings that every match must contain, with the method of `pg_trgm`'s regular expression support, and uses their grams in the same way. A pattern with no required literal gets no pruning.

**`pg_trgm` indexes are separate.** `USING gin (url gin_trgm_ops)` builds a real GIN index of section 12.5.3 from the `pg_trgm` code in `rupg-contrib`. The planner can use it and the gram summaries.

### 12.9.3 Cost

For Q20 the work is the gram intersection, the pattern test on the candidate strings, and a bit test for each of 100 million codes. The last part is the largest: 6.25 million codes for each vCPU at about 0.5 ns each is about 3 ms (a prediction). The first two parts depend on the distinct URL count, which document 09 measures at M4. The gram structures of a column must stay within `rupg.gram_budget`, a share of its compressed bytes with default 0.10. Above it, the loader builds no gram index and the executor uses the dictionary path.

## 12.10 Budgets and the honesty rule

The honesty rule of document 02 applies to every structure of sections 12.3, 12.6, 12.8 and 12.9. Every structure is built from the data only.

| Structure | Build time counted in | Bytes counted in | Under writes |
|---|---|---|---|
| segment summary, zone maps | mover and load | the segment | delete correction, section 12.6.2 |
| segment synopsis | mover and load | the segment | bounds stay valid |
| table synopsis | maintenance and load | the table extent | bounds plus hot count, section 12.8.2 |
| dictionary gram index | load and dictionary growth | the table extent | grows with the dictionary |
| vector gram filter | mover and load | the segment | immutable with the segment |
| sparse index part | mover and load | the index | dense part covers the rest |

**The delete test is a suite run** (document 21 section 21.8). It runs twice: once with `rupg.use_summaries = off`, which proves the planner does not need them, and once on a file in which the loader wrote no optional summaries, which proves the reader does not need them.

**The budgets are bounded.** `rupg.gram_budget` is a share of compressed column bytes. The synopsis has rudb's fixed caps of 512 entries and 65,536 candidate ordinals. The function `rupg_summary_size(regclass)` reports all summary bytes of a table, and they count in the data size of document 02.

## 12.11 Shards and indexes

Each shard has its own instance of each index over the rows of that shard (document 18). A non-unique index needs nothing more. A query that uses it runs on each shard, and document 18 gathers the results.

**A unique index must enforce uniqueness across all shards.** On a single node PostgreSQL places no limit on unique constraints, and rupg must place none either. In a cluster a unique index whose key contains the shard key is checked inside one shard, because equal keys go to the same shard. A unique index whose key does not contain the shard key needs a check in every shard or a global index. Citus refuses such constraints. rupg must not refuse them, because a schema that works on one node must work in a cluster. The design is a global unique index, sharded by its own key, updated in the same distributed transaction as the row. It costs one extra participant in the commit for each insert. Document 24 keeps open whether rupg refuses these constraints at M11 and adds global indexes at M12 (question 3).

## 12.12 Milestones and tests

| Milestone | Index work |
|---|---|
| M1 | `rupg-tree` with its crash tests, used by the hot store |
| M3 | B-tree dense form for primary keys and unique constraints, unique checks, the point path probe, delete marks and purge |
| M4 | segment summaries, zone maps, pruning and answering (H1), segment synopses, gram filters and the dictionary path (H4 at 1 million rows) |
| M5 | predicate locks on index ranges for `SERIALIZABLE` |
| M6 | every family of section 12.5, the sparse form, `CONCURRENTLY`, `amcheck`, operator classes, the property functions |
| M8 | table synopses and the certificate (H3), dictionary gram indexes (H4 at 100 million rows), the measurements of sections 12.8.5 and 12.9.3 |
| M11 | indexes in a cluster, section 12.11 |

**Each family has three kinds of tests.** The PostgreSQL regression files for the family run in the suite of document 21. A differential test runs random queries with the index and with `enable_indexscan`, `enable_bitmapscan` and `enable_indexonlyscan` off and compares the answers. A crash test kills the process during a build, an insert and a purge, recovers, and runs `check`.

**The summaries have the delete test.** Section 12.10 gives it. It runs on every pull request on the 1 million row ClickBench subset and on the TPC-H SF1 data.

## 12.13 Open questions

1. Whether rupg keeps the B-tree key limit of 2,704 bytes, which matches PostgreSQL, or uses the larger limit that 16 KiB pages allow (section 12.4).
2. Whether count mode meets the C3 budget on the 100 million row data set. The cost of section 12.8.4 is a prediction, and the first M8 measurement decides it.
3. Whether a unique constraint without the shard key is refused in a cluster at M11, with global unique indexes at M12 (section 12.11).
4. Whether the sparse form needs a merge policy other than "merge when a probe visits more than four segments", which is a budget with no measurement (section 12.3.2).
5. Whether the threshold of 1,024 distinct values for a synopsis column is right. It comes from the dense array size, not from a measurement (section 12.8.1).
