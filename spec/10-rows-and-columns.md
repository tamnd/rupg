# 10. Rows and columns

Written 7 October 2026.

This document specifies how a table is stored. A table has a hot store of row pages and a cold store of compressed column segments, and every row has a row id that it keeps for its life. This document gives the mapping from row ids to segments, the hot store, the segment layout with its delete marks, how a reader combines the two stores under one snapshot, how an update or delete changes a cold row, the mover, the sort order and sorted projections chosen at load, the bulk load, compaction, TOAST, system columns, partitioning, unlogged and temporary tables, the storage parameters, and the gates. Document 08 gives the pages and extents. Document 09 gives the encodings and summaries. Document 11 gives the transaction rules that this document uses. The code is in rupg-hot for the hot store, rupg-column for the cold store and its zone maps, and rupg-table for the mover, TOAST and system columns, as document 04 section 4.9 and document 22 give.

## 10.1 Two stores, one table

A table in rupg is not a row table or a column table. Every table has both stores, and the data of a table moves from the first to the second as it stops changing. A table that never stops changing stays in the hot store. A table that is bulk loaded and never changed goes directly to the cold store.

| Property | Hot store | Cold store |
|---|---|---|
| Structure | B+tree of 16 KiB PAX leaf pages keyed by row id | immutable segments of up to 262,144 rows |
| Change | in place, with undo | delete marks only, a changed row moves to the hot store |
| Versions | a version header on each row, older versions in undo | one version for each row, visibility by segment and delete marks |
| Compression | none, except TOAST values | the encodings of document 09 |
| Summaries | none | the segment summaries of document 09 section 9.7 |
| Written by | the page writer, out of place | the mover and the bulk load, as new extents |
| Main readers | the point path, short transactions | scans, aggregates, joins |

The design follows Colibri (PVLDB 17, 2024), which kept hot rows in PAX pages and cold rows in compressed blocks of up to 262,144 rows and measured about 10 times the throughput of the systems it compared on hybrid workloads. Colibri stored TPC-H at scale factor 1000 in 354 GB of cold blocks and needed only 41 MB of hot buffer pages, which shows how small the hot store is when most data is stable. The segment size of 262,144 rows is the same as Colibri's block size and is fixed by document 04 section 4.7.

**At any snapshot, a row id has at most one visible version.** This is the invariant of the two stores. Every operation in this document that moves a row between the stores keeps it: it deletes the row from one store and inserts it into the other in one transaction with one commit timestamp. A reader at a snapshot before that timestamp sees only the old place, and a reader at or after it sees only the new place.

## 10.2 Row ids and the segment map

### 10.2.1 Row ids

A row id is 64 bits: the shard number in the high 16 bits and a local number in the low 48 bits (document 04 section 4.7). The local number comes from a counter for each table and shard. 48 bits allow 281 trillion rows in one table on one shard.

**Workers take row ids in blocks.** A counter that every insert increments would be one cache line that every core writes. Each worker takes a block of 1024 local numbers from the table counter and gives them to its own inserts. The counter is in the catalog and its change is logged once for each block. A block that a worker does not finish, because the server stops or the worker moves to another table, leaves a gap. Gaps are normal: they also come from aborted inserts and from deleted rows. The block size of 1024 is a budget for M3, to be measured against the insert rate of YCSB and TPC-C.

**Workers insert into separate leaves.** Two workers with different blocks insert into different key ranges of the B+tree, so they write different leaf pages. A table with one inserting session has one active leaf, as in any B+tree keyed by an increasing number.

**A bulk load assigns row ids after it sorts.** Section 10.8 sorts the rows of a load before it assigns their row ids. The row id order of a bulk-loaded segment is therefore its sort order, and the segment needs no permutation (section 10.2.3).

### 10.2.2 The segment map

Each table and shard has a segment map. It is a B+tree in 16 KiB pages, keyed by the first row id of each segment. An entry has these fields.

| Field | Size | Meaning |
|---|---|---|
| first row id | 8 | lowest row id in the segment |
| last row id | 8 | highest row id in the segment |
| segment id | 8 | unique in the table and shard, never reused |
| publish timestamp | 8 | commit timestamp of the transaction that published the segment |
| retire timestamp | 8 | commit timestamp of the transaction that replaced it, or the maximum |
| directory page | 8 | logical page number of the segment directory |
| row count | 4 | rows in the segment |
| kind | 1 | base, patch or projection run |
| flags | 3 | sorted with permutation, has delete marks |

A reader at snapshot `s` sees an entry when its publish timestamp is at most `s` and its retire timestamp is above `s`. Compaction replaces segments by retiring old entries and publishing new ones in one transaction, so readers before and after the change each see a complete set.

**Ranges can overlap, and the map handles it.** A base segment from the mover or a bulk load covers a range of row ids. A row that leaves the cold store by an update (section 10.5) and later goes back by the mover lands in a patch segment, whose row id range overlaps older base segments. A lookup of row id `r` at snapshot `s` finds every visible entry whose range covers `r`, newest publish timestamp first, and takes the first segment that holds `r` with no visible delete mark. In the normal case there is one candidate. Compaction (section 10.9) merges patch segments back into base segments, which keeps the number of candidates small. The target is at most 2 candidates for 99 percent of lookups, measured at M5.

### 10.2.3 From row id to position

Each segment stores its row ids as a column, in segment order. For a segment in row id order with few gaps, this column is `DELTA` with almost every delta equal to 1, which is a few bytes for the segment (document 09 section 9.2). The position of row id `r` is found by binary search on the per-vector bounds of the row id column and then inside one vector.

A segment that the mover sorted by the table's sort key (section 10.7) is not in row id order. It stores a second structure, the row id index: the row ids sorted, with the position of each, both bit-packed. A position in a segment needs 18 bits, so the index costs about 3 bytes for each row before compression. This cost is in the bytes that gate G8 counts. Bulk-loaded segments do not need the index, because their row ids follow their sort order.

## 10.3 The hot store

The hot store of a table and shard is a B+tree keyed by row id. Inner nodes hold row id separators and child logical page numbers. Leaves hold rows in PAX layout. Readers descend with the optimistic reads of document 08 section 8.8.2.

### 10.3.1 The PAX leaf

A leaf is a 16 KiB page with the 64-byte page header of document 08 section 8.3.1. Its body has these parts in order.

| Part | Content |
|---|---|
| leaf header | row count, first row id, high key, right sibling, schema version |
| row id array | the row ids, sorted, 8 bytes each |
| version headers | one 16-byte header for each row |
| xmin minipage | hidden, the 32-bit `xmin` of each row, 4 bytes each (document 11 section 11.9) |
| xmax minipage | hidden, present only when a row in the leaf has a deleter or a locker (document 11) |
| null bitmaps | one bitmap for each nullable column |
| column directory | for each column, the offset of its minipage, 4 bytes each |
| fixed minipages | for each fixed-width column, its values as an array |
| variable heap | the bytes of variable-width values, with offsets in their minipages |

A scan of the hot store reads one minipage for each column it needs, so the hot part of a table is scanned with the same vector kernels as the cold part. A point lookup finds the row by binary search on the row id array and then reads one value from each minipage.

**The version header is 16 bytes.** It holds a timestamp word, a reference to the newest undo record of the row, and flags. The timestamp word is the commit timestamp of the version, or the transaction id of a writer that has not committed. Document 11 gives the exact encoding and the undo records, which follow the scalable snapshot isolation design of Alhomssi and Leis (PVLDB 16, 2023). The flags of this document are these: deleted, moved to cold, prior version in cold, and has TOAST values. PostgreSQL spends 23 bytes on the tuple header and 4 bytes on the line pointer for each row (PostgreSQL 19 documentation, database page layout), 27 bytes in total, against 28 here with the row id and the hidden `xmin` included.

**An update changes the row in place.** The old values go to undo, and the new values are written into the minipages. A fixed-width value is overwritten. A variable-width value that grows takes new space in the variable heap. If the page has no room, it is compacted. If it still has no room, it splits at a row id boundary. The row id never changes, so no index changes for an update that does not change indexed columns.

**A schema change does not rewrite pages.** `ALTER TABLE ... ADD COLUMN` with a constant default records the default in the catalog with a new schema version, as PostgreSQL does since version 11. A leaf with an older schema version reads the default for the missing column. The page is converted when it is next changed.

**Limits are those of PostgreSQL.** A table has at most 1,600 columns (PostgreSQL 19 documentation, appendix K). A row that does not fit in a leaf after TOAST fails with the same error that PostgreSQL gives for a row that is too big, SQLSTATE `54000`. A PAX leaf with many columns has a large column directory: at 1,600 columns it is 6,400 bytes, so very wide tables hold few rows in each leaf. That is acceptable, because a wide table is normally analytic and lives in the cold store.

## 10.4 Segments, delete marks and reads under one snapshot

### 10.4.1 The segment directory

A segment is a set of extents (document 08 section 8.4) and one segment directory. The directory is a chain of 16 KiB pages of kind 9. It holds these fields.

| Field | Content |
|---|---|
| identity | table OID, shard, segment id, kind, schema version |
| rows | row count, first and last row id, sort key id |
| row ids | the encoding tree and extents of the row id column, and of the row id index if any |
| xmin | the encoding tree and extents of the xmin column (section 10.11) |
| for each column | encoding tree, symbol table reference, dictionary version, extent list, vector offsets, and one checksum for each 64 KiB block |
| summaries | the location of the segment's summaries in the summary arena (document 09 section 9.7) |

The directory pages are mutable pages only until the segment is published. After that they never change, except that compaction and file shrink can move extents, and then a new directory replaces the old one in a transaction.

**A segment holds one version of each row and one timestamp for all of them.** Every row in a segment became visible at the publish timestamp of the segment. The mover moves only committed rows, and the bulk load publishes its rows at its commit. So a segment needs no visibility information for each row except its delete marks.

### 10.4.2 Delete marks

A delete mark records that a row of a segment was deleted, by a delete or by an update that moved the row to the hot store. Marks live in mutable pages, because segments are immutable.

**Recent marks carry a timestamp.** Each table and shard has a delete tree, a B+tree keyed by segment id and position. An entry holds the timestamp word of the delete: the commit timestamp, or the transaction id while the deleting transaction has not committed. An entry with a transaction id is also the row lock of the cold row: a second writer that finds it waits, as document 04 section 4.3 describes for hot rows.

**Old marks become bits.** When the oldest active snapshot is newer than a mark's commit timestamp, every reader sees the delete, and the timestamp is no longer needed. `VACUUM` and the autovacuum task then move the mark into the segment's delete bitmap and remove it from the delete tree. The bitmap is 262,144 bits, 32 KiB, two pages of kind 8, and a segment has one only when it has a settled delete. A bitmap is changed in place in the buffer pool and written out of place, like every page, and the change is logged.

| Mark state | Where | A reader at snapshot `s` |
|---|---|---|
| no mark | | sees the row |
| uncommitted, other transaction | delete tree, transaction id | sees the row |
| uncommitted, own transaction | delete tree, transaction id | does not see the row |
| committed at `t` | delete tree, timestamp | sees the row if `t` is above `s` |
| settled | delete bitmap | does not see the row |

### 10.4.3 A scan at one snapshot

A scan of a table at snapshot `s` does three things.

1. **It reads the visible segments.** It takes the entries of the segment map that are visible at `s`. For each segment it uses the summaries and zone maps to choose vectors (document 09 section 9.7 and document 12 section 12.6), reads them, and removes rows with a visible delete mark. A segment with no marks and no settled bitmap needs no check, and the summary records which segments have marks.
2. **It reads the hot store.** It scans the hot leaves in row id order. For each row it finds the version visible at `s` through the version header and undo. It skips rows that are deleted or moved at `s`.
3. **It combines the two.** The results are a union. The invariant of section 10.1 guarantees that no row id appears in both. The scan returns segments first and hot rows after, with no promise of order, as PostgreSQL makes no promise of order without `ORDER BY`.

A point lookup by row id at snapshot `s` probes the hot store first. If it finds a version visible at `s`, it returns it. Otherwise it probes the segment map as in section 10.2.2. Both probes are needed only when the hot store has no visible version, which is the case of a row that is in the cold store.

## 10.5 Update or delete of a cold row

A row in the cold store cannot change in place, because segments are immutable and compressed. An update of a cold row deletes it in the cold store and inserts the new version in the hot store under the same row id. Document 04 section 4.3 gives this rule. This section gives the steps for a transaction `T` at snapshot `s`.

1. **Find the row.** An index or a scan gives the row id. The lookup of section 10.2.2 gives the segment and position.
2. **Check the delete tree.** If an entry exists with another transaction's id, `T` waits for that transaction, as for a locked hot row. If an entry exists with a commit timestamp above `s`, the row was changed after `T`'s snapshot. In `REPEATABLE READ` and `SERIALIZABLE`, the statement fails with SQLSTATE `40001`. In `READ COMMITTED`, the statement reads the newest version of the row, which is in the hot store under the same row id, and evaluates its condition again, as PostgreSQL does when it follows an update chain. The stable row id makes this a hot store lookup, not a chain walk.
3. **Write the delete mark.** `T` inserts an entry with its transaction id into the delete tree. The insert is logged. The entry is now the row lock.
4. **For a delete, stop here.**
5. **For an update, read the old row.** `T` reads every column of the row from the segment with the `gather` operation of document 09 section 9.6. Each column costs one 64 KiB block read when the block is not in the buffer pool.
6. **Insert the new version.** `T` applies the update to the old values and inserts the result into the hot store under the same row id, with the flag "prior version in cold". The insert is logged like any hot insert.
7. **Commit.** The commit timestamp applies to both the delete mark and the hot version, so the invariant of section 10.1 holds. If `T` aborts, undo removes the hot version and the delete tree entry.

**Indexes do not change unless an indexed column changes.** The row id is the same, so every index entry that points to it is still correct. An index on a changed column gets a new entry, and the old entry is removed when no snapshot can see the old value, as document 12 specifies.

**A cold update costs more than a hot update.** Step 5 reads one block for each column of the row. A table of 20 columns pays up to 20 block reads for the first update of a cold row, against one leaf read for a hot row. The mover policy of section 10.6.2 exists to keep this case rare. Document 14 section 14.11 sets the target for interference between short transactions and analytic work, and the cold update rate is one of its inputs.

## 10.6 The mover

The mover is a maintenance task in queue 3 of document 04 section 4.5. It runs for each table and shard. It encodes stable ranges of hot rows into segments and publishes them in one transaction, as document 04 section 4.4 decides.

### 10.6.1 The steps

1. **Choose a range.** The mover walks the hot leaves from the lowest row id. It takes a range of row ids `[a, b)` in which every row has a committed version older than the stable age (section 10.6.2), and which holds a full segment of rows, 262,144. `VACUUM FULL` and `REPACK` also move a final range that is smaller than a segment.
2. **Take a snapshot.** It takes snapshot `m0` and reads the visible version of every row in the range. Rows deleted at `m0` are not read and leave gaps.
3. **Encode.** It sorts the rows if the table has a sort key (section 10.7), assigns dictionary codes, chooses encodings and builds summaries (document 09). It writes the extents and syncs them, as document 08 section 8.6.3 requires. No lock is held during this step, and writers continue to change rows in the range.
4. **Publish.** It starts transaction `M`. For each row in the range, it checks that no version newer than `m0` was committed and no writer holds the row. A row that changed is not moved: `M` writes a delete mark for its position in the new segment, at birth, and the hot version stays live. For every other row, `M` marks the hot version as moved, which is a logical delete in the hot store with undo. `M` publishes the segment in the segment map with publish timestamp equal to its commit timestamp, appends the new dictionary entries, logs the segment record of document 08 section 8.6.3, and commits.
5. **Reclaim.** When the oldest active snapshot is newer than `M`'s commit timestamp, no reader can need the moved hot versions. The mover removes the leaves of the range from the B+tree and frees their pages. The pages become reusable after the next checkpoint (document 08 section 8.9.2).

**Writers are never blocked by the mover.** Step 4 checks rows optimistically and excludes the rows that changed. A range in which more than 1 percent of rows changed during step 3 is abandoned, its extents are freed, and the range is tried again after the stable age. The threshold is a budget.

**Moved rows keep their row ids.** The segment's row id column holds them. Indexes are not changed by a move.

### 10.6.2 When to move

The mover moves a range only when moving pays for itself. These rules decide. The numbers are budgets for M4, to be tuned at M5 against TPC-C and at M8 against the mixed workload of document 14 section 14.11.

**A row is stable when its newest version is older than `rupg.mover_stable_age`.** The default is 60 seconds.

**A table moves only when its hot part is large.** The mover acts when the stable rows of a table and shard fill at least one segment, 262,144 rows, and the hot store of the table is larger than `rupg.mover_min_hot`, default 64 MiB. A small table that fits in memory stays in the hot store, where its updates are cheap.

**A table with many cold updates moves less.** The mover counts cold updates (section 10.5) for each table. When the cold updates in the last hour pass 1 percent of the rows moved in that hour, the stable age of that table doubles, up to 24 hours. When they fall below 0.1 percent, it halves, down to the default. TPC-C updates `stock` rows at random across each warehouse, so a fixed stable age would move rows that are then updated cold. The rule adapts the age to the measured update pattern. It looks only at the table's own writes, not at queries, so rule 1 of document 02 section 2.4.5 holds.

## 10.7 Sort order and sorted projections chosen at load

H5 of document 02 section 2.4.4 requires that the load path chooses a sort order for each table and up to two sorted projections from the data, without the queries. A range filter on the leading key then reads few vectors, because the per-vector bounds of a sorted column do not overlap.

### 10.7.1 The sort key

**The schema decides first.** If the table has an index marked by `CLUSTER`, its columns are the sort key. Otherwise, if the table has a primary key, its columns are the sort key. Rule 1 of document 02 section 2.4.5 allows a rule that looks at the schema. The ClickBench schemas for ClickHouse, AlloyDB, Aurora PostgreSQL and Citus declare a primary key on `hits` of `CounterID`, `EventDate`, `UserID`, `EventTime` and `WatchID` (ClickBench `clickhouse/create.sql`, `alloydb/create.sql`, `aurora-postgresql/create.sql` and `citus/create.sql` at `e6bda4e`). The plain `postgresql/create.sql` at the same commit declares no primary key. The rupg entry uses the plain PostgreSQL schema, so the data rule below decides its sort key.

**Otherwise the data decides.** On the sample of the first load, at least 65,536 rows, the loader evaluates candidate columns: integer, date, timestamp and dictionary-coded string columns with at least 2 distinct values and at most one distinct value for every 64 rows. For each candidate it sorts the sample by that column and runs the chooser of document 09 section 9.3 on all columns. It keeps the column that gives the smallest estimated table size. It then repeats with the remaining candidates as a second key and a third key, and stops when a key reduces the estimate by less than 2 percent. The thresholds are budgets for M4.

The size is the measure because it depends only on the data, and because a sort that makes runs and narrow ranges for compression also makes narrow per-vector bounds for filters. The sort order is recorded in the catalog. `CLUSTER`, `REPACK` and the storage parameter `rupg.sort_key` change it, and the next rewrite applies the new order.

**A load sorts in runs, not globally.** A bulk load sorts its rows in runs of up to 16 segments, 4,194,304 rows, and writes each run as consecutive segments in sort order. A global sort of a large load would need a spill of the whole input, and the load budget of document 09 section 9.4 has no time for it. Inside a run, the per-segment bounds of the leading key are narrow. Across runs, they overlap. Compaction merges runs in the background (section 10.9). Whether the load must sort globally for the cold run target is an open question in document 24.

### 10.7.2 Sorted projections

A sorted projection is a covered set of columns, an order, and a row identity, with no aggregates (rudb `storage-v3/35`). It is a second copy of some columns in another order. rudb's prototype ordered `hits` by `UserID` and covered `RegionID`, at 10 bytes for each row, 99,997,500 bytes. Q9 took 4.635 ms and 4.98 MiB, against DuckDB's 93.390 ms and 426.05 MiB, in 51 runs with 16 workers.

**The rule chooses at most two projections from the data.** It runs after the sort key is chosen, on the same sample.

1. **Candidate keys.** Integer and dictionary-coded columns that are not in the sort key, with a distinct count between one for every 10,000 rows and one for every 2 rows.
2. **Pruning loss.** For each candidate, the rule computes how many vectors of the table, sorted by the sort key, a lookup of one value of the candidate would read, from the per-vector bounds of the sample. A candidate that the sort order already clusters reads few vectors and is dropped.
3. **Covered columns.** For each remaining candidate key `K`, a column `C` is covered when the distinct count of the pair `(K, C)` in the sample is at most 1.1 times the distinct count of `K`, which means that `C` is almost a function of `K`.
4. **Choice.** The rule keeps the two candidates with the highest pruning loss whose projections fit in the byte budget: all projections of a table together at most 5 percent of the table's encoded bytes. At 7.67 GB, 5 percent is 384 MB, and the rudb prototype used 100 MB.

The rule never looks at the queries. `UserID` and `RegionID` in `hits` are a pair that step 3 finds from the data, because a user is almost always in one region. Whether the rule finds that pair on the full table is measured at M8.

**A projection is stored as sorted runs.** Each run covers a set of base segments: one bulk load, or the segments of up to 16 moves. A run is a segment of kind projection run in the segment map, with the key, the covered columns and the row identity as columns. The row identity is the base segment id and position, not the row id, so that a reader checks the base segment's delete marks directly. A range filter on `K` reads each run's per-vector bounds and the matching vectors. Compaction merges runs when a table has more than 8, and rebuilds a run when a base segment that it refers to is replaced.

**Projections are maintained transactionally.** A run is published in the transaction that publishes its base segments. A delete of a base row is seen through the base delete mark, so a projection never returns a deleted row. Rows in the hot store are not in any projection, so a scan through a projection adds the hot rows that match, as in section 10.4.3. This meets rule 5 of document 02 section 2.4.5. Deleting a projection changes no answer, only time, which is rule 4.

Document 12 specifies how the planner chooses a projection as an access path and how projections appear in the catalog.

## 10.8 Bulk load

`COPY FROM` and `INSERT ... SELECT` write segments directly when they load many rows, as document 04 section 4.4 decides. The bulk path is a different destination, not a different statement. Constraints, defaults, generated columns and triggers run as in PostgreSQL.

**The bulk path starts at one segment.** A statement buffers its first rows in memory. When it has 262,144 rows, it switches to the bulk path for the whole statement. When it ends with fewer, it inserts them into the hot store. The threshold is the segment size, so a bulk load never makes a segment smaller than a full one except its last.

**The steps of a bulk load are these.**

1. **Parse in parallel.** Text and CSV input is split at record boundaries among the workers. A CSV quote can hide a newline, so the split is speculative and checked, as in the speculative CSV parsing of Ge, Li, Eilebrecht, Chandramouli and Kossmann (SIGMOD 2019). An error is reported with the line number of the first bad record in input order, as PostgreSQL reports it, so a client sees the same `CONTEXT` line.
2. **Run constraints and triggers.** Defaults, generated columns, `CHECK` constraints and row triggers run for each row. `NOT NULL` checks run on vectors.
3. **Sort a run.** When a run of 16 segments is full, or the input ends, the loader sorts the run by the sort key (section 10.7) and assigns row ids from the table counter in sort order.
4. **Encode.** For each column of each segment, assign dictionary codes, choose the encoding on the sample, encode, and build summaries and projection runs (document 09).
5. **Write.** Write the extents with direct I/O, not through the buffer pool, and sync them (document 08 section 8.6.3).
6. **Index.** Insert index entries in row id order. For an empty table, build each index from the sorted keys instead, which is faster than inserts.
7. **Publish at commit.** The segments, projection runs and dictionary entries are published by the commit of the loading transaction. Before the commit, only that transaction sees them.

**Unique constraints and foreign keys are checked as in PostgreSQL.** A unique index is checked for each row, in batches of one vector. A foreign key is checked for each row at the end of the statement, as the PostgreSQL trigger does, in batches.

**Memory is bounded.** The loader holds, for each worker, one run of sort keys with positions and the column buffers of the segment it encodes. For input in row form, the loader writes the transposed columns of a run to a spill arena and reads them back for the sort. The bulk path reserves its memory from `rupg.memory_limit` like any operator, and spills when the reservation is refused. The memory and time numbers are measured at M4 against the load target of document 02 section 2.4.8 and the method of document 20 section 20.3.

**Parquet is a source at M9.** rupg-import reads Parquet files and feeds the same steps, with step 1 replaced by a column reader. Until M9, the ClickBench load runs through `COPY`.

## 10.9 Compaction and VACUUM

`VACUUM` in rupg does compaction, as document 00 decides. It accepts every option of PostgreSQL's `VACUUM` and reports in the same views.

**Plain `VACUUM` and autovacuum do this work for a table.**

1. Remove undo that no snapshot can see, as document 11 specifies.
2. Settle delete marks into delete bitmaps (section 10.4.2).
3. Reclaim hot leaves of moved ranges (section 10.6.1 step 5).
4. Rewrite a segment whose deleted share passes 20 percent, merging it with neighbours so the new segments are full. The threshold is a budget.
5. Merge patch segments into base segments when a row id range has more than 2 visible candidates (section 10.2.2).
6. Merge projection runs when a table has more than 8 (section 10.7.2), and merge sort runs of the base when the per-segment bounds of the leading key overlap in more than 4 segments for a value.
7. Rebuild a dictionary when its unreferenced entries pass one eighth, or sort its tail when it passes one sixteenth of the base (document 09 section 9.5).
8. Rewrite the table's summaries in column order when they are spread over more than one summary arena (document 08 section 8.9.1).

Each rewrite publishes the new segments and retires the old ones in one transaction. Old extents are freed when no snapshot sees them (document 08 section 8.9.2). Autovacuum starts this work under the thresholds of the PostgreSQL settings `autovacuum_vacuum_threshold`, `autovacuum_vacuum_scale_factor` and their table storage parameters, where a dead row is a row with a settled or committed delete mark or a dead hot version.

**`VACUUM FULL`, `CLUSTER` and `REPACK` rewrite the whole table.** They sort the whole table by its sort key, rebuild its dictionaries with a sorted base, rebuild its projections and summaries, and move it to low arenas so the file can shrink (document 08 section 8.9.3). They take the lock that PostgreSQL takes for the same command, so clients see the same blocking.

**The statistics views count the same things.** `pg_stat_user_tables.n_live_tup` and `n_dead_tup`, `last_vacuum` and `last_autovacuum` are kept for each table. `n_dead_tup` counts unsettled delete marks and dead hot versions.

## 10.10 TOAST

TOAST applies to the hot store. Document 09 section 9.9 gives the thresholds and the compression, lz4 by default.

**Each table with a TOAST-able column has a TOAST tree.** It is a B+tree in 16 KiB pages of kind 10, keyed by a 64-bit value id and a 32-bit chunk number. A chunk holds up to 16,256 bytes, which leaves 128 bytes of the page for the page header, the chunk key and the slot data. A value in a hot row that is stored out of line is replaced by an 18-byte pointer that holds the raw size, the stored size, the compression method and the value id, which is the size of PostgreSQL's TOAST pointer (PostgreSQL 19 documentation, TOAST). The largest value is 1 GB, as in PostgreSQL.

**The TOAST tree follows the row's transactions.** A value written by an update gets a new value id. The old value is removed when no snapshot can see the old row version. An update that does not change a TOAST value keeps the pointer, so it does not copy the value.

**The catalog shows a TOAST table.** `pg_class.reltoastrelid` of a table points to a relation in the `pg_toast` schema, and tools that list relations see it, as in PostgreSQL. The relation is a view of the TOAST tree made by rupg-pgcatalog (document 07).

**The mover detoasts.** When a row moves to the cold store, its TOAST values are read whole and encoded with the column (document 09 section 9.9). The TOAST chunks are removed when the moved hot version is reclaimed. A cold row that is updated comes back to the hot store with its large values TOASTed again.

## 10.11 System columns

System columns are computed, not stored as PostgreSQL stores them (document 00). This table gives the value of each one.

| Column | Hot row | Cold row |
|---|---|---|
| `tableoid` | the table OID | the table OID |
| `ctid` | from the row id, below | from the row id, below |
| `xmin` | low 32 bits of the transaction id of the version, from the version header and undo | low 32 bits of the transaction id stored in the segment's xmin column |
| `xmax` | the transaction id of a deleter or locker that has not finished or is not visible, else 0 | the transaction id in the delete tree entry, else 0 |
| `cmin`, `cmax` | the command id for the current transaction's own changes, else 0 | 0 |

**The segment stores an xmin column.** A cold row must show the `xmin` of the transaction that wrote its current version, because client libraries use `xmin` as a concurrency token. The Npgsql provider for Entity Framework Core documents `xmin` for optimistic concurrency. The mover copies the transaction id of each moved version into the column. A bulk load writes one value for all its rows, which is a `CONST` vector. The bytes count in the disk gates.

### 10.11.1 The ctid decision

PostgreSQL's `ctid` is the physical location `(block, offset)` of a row version. It changes when the row is updated and when `VACUUM FULL` or `CLUSTER` moves it. rupg has no physical location for a row: a hot row moves between pages by splits and out-of-place writes, and a cold row is a position in a compressed segment.

**Decision: `ctid` is computed from the row id and does not change on update.** For local number `n`, the block is `n` divided by 256 and the offset is `n` modulo 256, plus 1. The block fits in the 32 bits of a PostgreSQL block number while `n` is below 2 to the power 40, about 1.1 trillion rows in one table and shard. The offset is between 1 and 256, which is below PostgreSQL's 291 rows for each 8 KiB heap page (`MaxHeapTuplesPerPage`), so tools that check the range of an offset accept it. In a cluster, `ctid` is unique within a shard, not across shards.

**These uses work.** `SELECT ctid` returns a valid `tid`. `WHERE ctid = $1` finds the row, with a lookup by row id. Range conditions on `ctid`, which PostgreSQL 14 made a TID range scan, find the rows in row id order. `tid_block()` and `tid_offset()` of PostgreSQL 19 return the two parts. The common idiom that deletes duplicates by keeping the row with the smallest `ctid` keeps the oldest row.

**This is the compatibility cost.** PostgreSQL returns a new `ctid` after an update, and rupg returns the same one. A program that detects a concurrent update by a change of `ctid` does not see the change. A program that reads `ctid` blocks to estimate the physical size or bloat of a table gets numbers that do not describe pages. The PostgreSQL documentation says that `ctid` is useless as a long-term row identifier because it changes, so no correct program depends on its change, but some programs may. `xmin` changes on every update in rupg as in PostgreSQL, which covers the common concurrency check. The decision and its cost are in document 24, and the compatibility suite of document 21 records each test that observes the difference.

## 10.12 Partitioning

A partition of a declaratively partitioned table is a table. It has its own row id counter, hot store, segment map, dictionaries, summaries, sort key and projections. `ATTACH PARTITION` and `DETACH PARTITION` change only the catalog. Partition pruning uses the partition bounds first and the segment summaries second.

**Codes are local to a partition.** Document 09 section 9.5.6 keeps one dictionary for each table, and a partition is a table. A group by on a string over several partitions therefore cannot compare codes across partitions. The executor groups each partition on its codes and merges the groups across partitions by string, or it builds a translation from each partition's codes to codes for the query, as document 14 decides. A shared dictionary for a partitioned table would avoid this cost and would make `ATTACH` and `DETACH` rewrite codes. The choice is an open question in document 24.

**Sharding is not partitioning.** A shard is a horizontal piece of a table across nodes and has its own row ids, as document 18 specifies. A partitioned table can also be sharded, and then each partition has one piece on each shard.

## 10.13 Unlogged and temporary tables

**Unlogged tables write no log records.** Their pages carry the unlogged flag (document 08 section 8.3.1), and their changes and segment publications do not go to the log rings. Their pages are written out of place and checkpointed like other pages. After a clean shutdown, the slot records it (document 08 section 8.2.2) and the table keeps its data. After a crash, recovery truncates every unlogged table to empty, as PostgreSQL does. Its segments are freed by the free space reconciliation of document 08 section 8.13.

**Temporary tables live in the spill arenas.** A temporary table is local to its session, as in PostgreSQL. Its pages are kept in session memory up to `temp_buffers` and then written to a spill arena (document 08 section 8.9.1). It writes no log and is never in a checkpoint. A temporary table with a large load uses the bulk path into segments in the spill arena, so an ETL session gets the same compression. Its space is freed at the end of the session, and every open frees all spill arenas.

## 10.14 Storage parameters

Document 02 decides that `CREATE TABLE ... USING columnar` and storage parameters are hints only. A hint can change the speed and size of a table. It never changes an answer.

| Parameter | Effect in rupg |
|---|---|
| `USING heap` | the default access method name, as in PostgreSQL, a normal rupg table |
| `USING columnar` | a normal rupg table with the hints `rupg.mover_stable_age = 0` and a bulk path threshold of one vector |
| `fillfactor` | the fill of hot leaves after a split |
| `toast_tuple_target` | the TOAST target of document 09 section 9.9 |
| `autovacuum_*`, `vacuum_truncate` | the triggers of section 10.9 |
| `parallel_workers` | the worker limit of a scan of the table |
| `user_catalog_table` | accepted and shown, for logical decoding |
| `rupg.sort_key` | a column list that replaces the rule of section 10.7.1 |
| `rupg.projections` | `auto` or `off`, for section 10.7.2 |
| `rupg.mover_stable_age` | an interval that replaces the adaptive age of section 10.6.2 |
| `rupg.dictionary` | a column option, `auto`, `on` or `off`, for document 09 section 9.5.1 |

`pg_am` lists `heap` and `columnar`, and `default_table_access_method` is `heap`. A client that creates a table with `USING columnar` and reads `pg_class.relam` gets the OID of `columnar`. Unknown storage parameters fail as in PostgreSQL, with SQLSTATE `22023`.

**The benchmark runs set no hint.** ClickBench, TPC-H and TPC-C run with the default of every parameter in this table, so that the result shows the rules of this document and not a choice for one workload. Document 20 states this rule for each suite.

## 10.15 Gates

| Milestone | What this document delivers | Gate or test |
|---|---|---|
| M1 | hot store B+tree with PAX leaves, row ids and their blocks, version headers, the Rust KV-level API | crash tests of document 08 section 8.16 |
| M3 | the point path on the hot store | G4, document 14 section 14.9 |
| M4 | segments, segment map, delete marks, the mover, the bulk load, sort key at load, compaction steps 1 to 5 | G2, G3, the load target |
| M5 | cold update and delete under all isolation levels, the adaptive mover | G5, the interference target of document 14 section 14.11 |
| M8 | sorted projections, compaction steps 6 to 8 | G7, G8 |
| M9 | Parquet import | |

Three tests run at every milestone from M4. **The invariant test** runs random inserts, updates, deletes, moves, bulk loads and compactions under concurrent snapshots, and checks at every snapshot that each row id has at most one visible version and that a scan returns the same rows as a model in memory. **The summary test** deletes every summary and projection and runs every suite again, as document 21 section 21.8 requires. **The cold update test** measures the time of an update of a cold row against a hot row on a table of 20 columns, and the report gives both numbers.
