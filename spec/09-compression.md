# 9. Compression

Written 7 October 2026.

This document specifies how rupg encodes column data in the cold store. It gives the encodings, the cascades and the chooser that picks them, the speed of the encoder, the global dictionaries of string columns, the contract between encodings and the executor, the segment summaries, the mapping from PostgreSQL types to physical types, the TOAST thresholds, and the gates. Document 08 gives the extents that hold the encoded bytes. Document 10 gives the segments, the mover and the bulk load that call the encoder. Document 14 gives the operators that run on encoded data. The encodings are lifted from rudb into rupg-encode, the zone maps and global codes into rupg-column, and the summaries into rupg-summary, as document 04 section 4.9 records.

## 9.1 What compression must buy

Compression in rupg serves four targets of document 02 at the same time. The table gives each target and what it asks of this document.

| Target | Number | Source | What it asks of compression |
|---|---|---|---|
| disk against PostgreSQL | at most 10.65 GB for `hits` | document 02 section 2.6.3 | ratio |
| gate G3 at M4 | below 9.45 GB, the ClickHouse size | document 02 section 2.10 | ratio |
| gate G8 at M8 | below 7.67 GB, the elosdb size | document 02 section 2.10 | ratio, with summaries counted |
| cold run | at most 2.78 GiB read over 43 queries | document 02 section 2.4.7 | ratio of the columns that queries touch, and small read units |
| load | at most 1.25 times the time to read `hits.parquet` | document 02 section 2.4.8 | encoder speed |
| hot run | 10x ClickHouse at M8 | document 02 section 2.4 | decode speed, and work on encoded data without decode |

The ratio targets and the speed targets pull in different directions. The strongest cascades give the smallest files and the slowest encoder. rudb measured this cost directly: its chooser found cascades that stored the full `hits` table in 9.65 GB, down from 11.65 GB, but the encoder ran at 5 MB per second per core, used 5.6 CPU hours for the table and peaked at 1002 MB of memory (`../2140/03-baselines.md` section 3.5.1). The ratio meets the PostgreSQL target, and the speed misses the load target by a factor that section 9.4 computes. The design below keeps the encodings that gave the ratio and changes how the encoder searches.

## 9.2 The encodings

Every encoding works on units of 1024 values, the vector size of document 04. A vector of any column decodes alone, given the segment metadata of that column: its dictionary, its symbol table and its encoding tree. No encoding carries state from one vector to the next. This rule lets a reader decode only the vectors that zone maps and summaries select (section 9.7).

| Name | Input | Output | Source |
|---|---|---|---|
| `CONST` | any | one value for the vector | rudb |
| `PLAIN` | any fixed width | the values as stored | rudb |
| `BITPACK` | unsigned integers | 0 to 64 bits per value, FastLanes unified transposed layout | FastLanes, VLDB 2025 |
| `FOR` | integers | integers minus a base | FastLanes |
| `DELTA` | integers | differences in the FastLanes transposed order | FastLanes |
| `RLE` | any | a value array and a run length array, each encoded again | FastLanes, rudb |
| `DICT` | integers, floats | codes into a dictionary local to the segment | rudb |
| `GCODE` | strings | 32-bit codes into the table's global dictionary, section 9.5 | rudb `storage-v3/09` |
| `FSST` | strings | byte strings compressed with a 255-symbol table | Boncz, Neumann and Leis, VLDB 2020 |
| `FRONT` | sorted strings | each string as a shared prefix length and a suffix | rudb `03-baselines` section 3.5.1 |
| `ALP` | floats | decimal integers with an exponent and a factor, plus exceptions | Afroozeh, Kuffó and Boncz, SIGMOD 2024 |
| `ALPRD` | floats | the front bits by dictionary, the back bits packed | ALP |
| `PATCH` | any | a list of positions and values that replace decoded values | FastLanes, ALP |
| `BOOL` | booleans | one bit per value | rudb |
| `ZSTD` | opaque bytes | a zstd frame for each vector | for types with no structure, section 9.8 |

Validity, which says which values are NULL, is stored apart from the values. It has three forms, as in rudb `storage-v3/02`: all valid, all null, or a bitmap. A bitmap with few set bits is stored as a list of positions with `PATCH`. A NULL position in the value stream holds a value that keeps the stream compressible, the previous value for `DELTA` and `RLE`, and code 0 for `GCODE`.

**FastLanes decides the integer layer.** FastLanes (Afroozeh and Boncz, VLDB 2025) measured decoding on average 43 times faster than Parquet with Snappy and 7 times faster than BtrBlocks, and 40 percent faster again with AVX-512. Its transposed layout lets one kernel unpack 1024 values with no dependency between lanes, so the same code is vectorized on AVX-512, AVX2, NEON and the 128-bit SIMD of WebAssembly. rupg-kernels has these four paths and a portable path, as document 04 section 4.10 requires. The cost that FastLanes reports is the encoder: about 14 times slower than Parquet with Snappy. Section 9.4 handles that cost.

**ALP decides the float layer.** ALP is exact. It finds a decimal form of each double, stores the integers with `FOR` and `BITPACK`, and keeps the values that have no decimal form as exceptions. DuckDB measured an average ratio of 4.3 on its float data sets when it adopted ALP (DuckDB pull request 9635). `ALPRD` covers columns of real doubles with full mantissas. ALP keeps every bit, so negative zero, infinities and every NaN payload return as they went in, which binary `COPY` requires.

**FSST decides the string layer, with the OptFSST trainer.** FSST compresses strings with a table of up to 255 symbols of up to 8 bytes and keeps random access to each string. A predicate on a string can run on compressed bytes by compressing its constant with the same table. OptFSST (ADMS 2026) changed only the training of the table and gained 7.3 percent of compression for FSST and 17.0 percent for the variant with 12-bit codes, with an unchanged decoder. rupg trains with OptFSST and decodes with the FSST decoder. The 12-bit variant is an open question in document 24, because it changes the decoder.

## 9.3 Cascades and the chooser

An encoding produces arrays, and each array can be encoded again. `DELTA` produces integers that `FOR` shifts and `BITPACK` packs. `GCODE` produces codes that `FOR` and `BITPACK` pack. `RLE` produces two arrays and each one gets its own tree. The result is an encoding tree for each column of each segment, which FastLanes calls an expression encoding. The tree is stored in the segment directory as a small serialized structure, and the decoder is a fold over it.

rudb's chooser found deep trees. Its tree for `URL` was `DICT(FRONT(DICT(DELTA(RLE(FOR+BITPACK, FOR+BITPACK))), FSST[255](DICT(DELTA(RLE(...))))), RLE(...))`. The depth found the ratio and also made the search expensive.

### 9.3.1 The candidates

**The candidate trees are a fixed list for each physical type, not a search.** BtrBlocks (Kuschewski, Sauerwein, Alhomssi and Leis, SIGMOD 2023) chose among 8 schemes for each block by sampling and reached ratios close to heavier formats with fast decoding. rupg uses the same approach with a list for each type.

| Physical type | Candidate trees |
|---|---|
| integers | `CONST`; `FOR+BITPACK`; `DELTA+FOR+BITPACK`; `RLE(values: FOR+BITPACK, lengths: FOR+BITPACK)`; `DICT(codes: BITPACK, dictionary: FOR+BITPACK)`; `FOR+BITPACK` with `PATCH` for outliers |
| floats | `CONST`; `ALP`; `ALPRD`; `RLE(values: ALP, lengths: FOR+BITPACK)`; `DICT(codes: BITPACK, dictionary: ALP)` |
| strings with a dictionary | `GCODE` with codes as an integer tree from the list above |
| strings without a dictionary | `FSST` with lengths as an integer tree; `PLAIN` with lengths as an integer tree |
| booleans | `CONST`; `BOOL`; `RLE` |
| opaque bytes | `ZSTD`; `PLAIN` |
| dictionary payloads | `FRONT` then `FSST` for sorted payloads; `FSST` for unsorted payloads |

**The tree depth is at most 3 below the root.** Each inner array is an integer array and uses the integer list, but without `DICT` and without a further `RLE`. The depth limit is a budget for M4. If the measured ratio on `hits` misses gate G3 because of the limit, a deeper candidate is added for the column type that misses, and the cost in encoder time is measured in the same change.

### 9.3.2 The sample

The chooser estimates each candidate on a sample of the segment column. The sample is 64 runs of 64 consecutive values, taken at evenly spaced positions across the segment, which is 4,096 values, 1.6 percent of a full segment of 262,144 rows. BtrBlocks also samples runs of consecutive values and not single values, because single values hide the runs that `RLE` and `DELTA` exploit. The positions are spread across the whole segment, because the first values of a sorted column look constant and would lead the chooser to `CONST`.

### 9.3.3 The cost model

For each candidate tree the chooser encodes the sample and measures its size. It then estimates the cost of one vector with this model.

`cost = S * r + D * m`

`S` is the estimated encoded bytes of one vector. `r` is 3.8 ns per byte, the time to read one byte from a gp2 volume at 250 MiB per second (document 02 section 2.4.7). `D` is the decode time of one vector for the tree, in ns, from a table that rupg-kernels measures for each kernel path when the binary is built. `m` is 0.5 when the executor runs filters and aggregates on the tree's encoded form, and 1 when it must decode first. The encoded forms with `m` equal to 0.5 are `CONST`, `FOR`, `BITPACK`, `RLE`, `DICT` and `GCODE` (section 9.6).

The chooser picks the tree with the lowest cost. The weight on decode comes from rudb, whose chooser weighted the decode term by the support for compressed execution (rudb `05-storage`). The weight on bytes is the read speed of the cold run, so the model picks the tree that minimizes the time of a cold read plus decode. At 3.8 ns per byte, a tree that saves 1 KiB of a vector can spend about 3.9 microseconds more on decode and still win. On the decode speeds that FastLanes reports, bytes almost always dominate, so the model favours ratio, which is what gates G3 and G8 need. `m` keeps the hot run from paying for that choice where the executor can avoid the decode.

**The model never looks at queries.** It looks only at the data and at the kernel table. This is rule 1 of document 02 section 2.4.5.

**The choice is deterministic.** The same rows in the same order give the same tree and the same bytes on every platform. The kernel table is therefore fixed in the binary for each release, not measured on the host. Replicas that run the mover on their own copy of a shard need this property, and document 18 depends on it.

## 9.4 Encoder speed

The load target is at most 1.25 times the time to read the source file (document 02 section 2.4.8). `hits.parquet` is 14,779,976,446 bytes. At 250 MiB per second its read takes 56.4 s, so the load has 70.5 s. The `c6a.4xlarge` has 16 vCPUs (AWS instance types), so the load has at most 1,128 vCPU seconds, about 0.31 CPU hours, for all its work: reading Parquet, decoding it, building dictionaries, choosing and encoding, writing summaries and syncing.

rudb used 5.6 CPU hours to encode the same table. rupg must be 18 times faster per row. This budget splits the 1,128 vCPU seconds.

| Step | Budget, vCPU seconds | Share |
|---|---|---|
| read and decode Parquet | 280 | 25 percent |
| dictionary inserts for string columns | 170 | 15 percent |
| chooser on samples | 60 | 5 percent |
| encode with the chosen trees | 450 | 40 percent |
| summaries, code sets and zone maps | 110 | 10 percent |
| writes, syncs and catalog | 58 | 5 percent |

The budget is not a measurement. It is the plan for M4, and the first M4 load run replaces it with measured numbers.

**These rules make the budget possible.**

**The chooser runs on the sample only, and the trees are shallow.** rudb's measurement of 5 MB per second per core does not say which step took the time, so the first task of M4 is to profile the lifted encoder on `hits`. rupg encodes each segment column once, under the tree that the sample chose, and section 9.3.1 limits the depth of that tree. With 64 runs of 64 values and at most 7 candidates for each type, the chooser encodes at most 28,672 values for a column of 262,144, about 11 percent of the encode work, and the budget above gives it 5 percent of the time because the sample trees are encoded without a write.

**Symbol tables are trained once for each table and column.** OptFSST trains on a sample of the first segment of a load. Later segments use the same table. A new table is trained only under the rule of section 9.5.6.

**Encoding is parallel by segment and by column.** A worker takes one column of one segment as a morsel in queue 3 of document 04 section 4.5. Each morsel is independent once the dictionary codes of the segment are assigned.

**The encoder streams.** A worker holds the values of one column of one segment, not the whole segment. For a 64-bit column that is 2 MiB. For rows that arrive in row form, from `COPY` text or from the hot store, the loader transposes 1024 rows at a time into column buffers. Document 10 section 10.8 gives the memory budget of a load.

## 9.5 Global per-table dictionaries

A string column of a table has at most one dictionary, shared by all segments of the table in one shard. A code therefore means the same string in every segment, so group by, join and distinct run on 32-bit codes and never compare strings (H2 of document 02 section 2.4.4). rudb measured Q34 going from 16.00 ms to 1.56 ms and Q35 from 19.00 ms to 1.52 ms with stable codes into one snapshot-wide dictionary (`storage-v3/09`).

### 9.5.1 When a column gets a dictionary

At the first load or the first move into the cold store, rupg estimates the distinct count of each string column with a HyperLogLog sketch over the sample of section 9.3.2 and over the first segment. A column gets a dictionary when the estimated distinct count is at most half of the row count. `URL` in `hits` has 27,374,884 distinct values in 99,997,497 rows (rudb `02-the-goal` section 2.6.1, ClickBench), a ratio of 0.27, so it gets a dictionary. A column with almost unique values, such as a text identifier, stores `FSST` without a dictionary. The threshold of one half is a budget for M4. `REPACK` and `VACUUM FULL` evaluate the decision again on the whole column.

Integer and float columns do not get global dictionaries. They use `DICT` local to a segment when the chooser picks it. Group by on integers already runs on integers.

### 9.5.2 Codes

A code is a 32-bit unsigned integer. Code 0 is reserved and means no value: the value stream holds code 0 at NULL positions, and validity says the value is NULL. NULL is not a dictionary entry. The empty string is an ordinary entry with its own code, so NULL and the empty string never mix, as rudb `storage-v3/11` requires. Code `0xFFFFFFFF` is reserved as a sentinel for lookups that find nothing. A dictionary therefore holds at most 4,294,967,294 entries. A column that reaches the limit stores new segments with `FSST`, and queries over its segments fall back from codes to strings for those segments.

On disk, codes are stored with the integer cascade, so a vector whose codes fall in a narrow range packs to few bits. In memory, the executor works on 32-bit codes.

### 9.5.3 The layout

A dictionary has a sorted base and an unsorted tail. The base is built by a bulk load, by `REPACK` or by `VACUUM FULL`, and its codes are in the order of the column's collation. The tail holds the strings added after the base, in the order they arrived. Codes of the base are 1 to `b`, and codes of the tail are `b + 1` and up.

The dictionary is stored in extents of an extent arena (document 08 section 8.4). It has the layout of rudb's dictionary page (`storage-v3/09`): an entry count, `count + 1` offsets, and the payload. The offsets are 64-bit, because the payload of a large column can pass 4 GiB. The payload of the base is `FRONT` then `FSST`, and the payload of the tail is `FSST`. Each 64 KiB block of the dictionary has its checksum in the dictionary header, the same read unit as every extent. rudb found that front coding cut `URL`, `Referer` and `OriginalURL` from 6.11 GB to 4.44 GB together (`../2140/03-baselines.md` section 3.5.1), which is why the base is sorted and front coded.

**A string is found by binary search in the base and a hash table on the tail.** The base is searched by decoding about `log2(b)` entries, which is 25 for `URL`. The tail has an in-memory hash table that is built when the dictionary is first used after open and maintained as the tail grows. No hash index is stored on disk, because for `URL` at 8 bytes per entry it would be 219 MB, about 3 percent of the 7.67 GB target, and it would rebuild faster than it reads at the cold read rate.

### 9.5.4 Growth and transactions

**A dictionary only grows between rewrites.** The mover and the bulk load add new strings to the tail in the transaction that publishes their segments (document 10 sections 10.6 and 10.8). A string gets its code when it is first added. Codes are never reused while any segment refers to them.

**An aborted transaction leaves its entries.** Entries that an aborted transaction added stay in the tail. No segment refers to them, and no query returns them, because a query reads codes only from segments that it can see. The cost is bytes. A deleted row also leaves its string in the dictionary until a rewrite. Both kinds of entry are removed by `REPACK`, `VACUUM FULL`, and by the compaction of document 10 when the unreferenced entries pass one eighth of the dictionary. A database that must erase a deleted value from the file must run `REPACK` on the table. This is an open question in document 24.

**The hot store stores strings, not codes.** A row in the hot store has its string bytes. The mover converts them to codes. A query that groups on a string column over hot and cold rows looks up each hot string in the dictionary, and a string that the dictionary does not have gets a temporary code for the query, above the dictionary's last code.

### 9.5.5 Order and collation

**A range predicate on a sorted base is a code range.** `URL >= 'http://a' AND URL < 'http://b'` becomes a range of base codes by two binary searches. Tail codes are tested one by one, once for each query, and the result is a code bitmap of the tail. The tail is small after a rewrite and grows with the mover. When it passes one sixteenth of the base, compaction schedules a rewrite of the column that sorts the dictionary and rewrites the codes in every segment. This rewrite is expensive, so its threshold is a budget to measure at M4.

**`ORDER BY` on codes uses a rank table.** When the tail is empty, the code is the rank. When it is not, the executor builds a rank for every code once for each dictionary version, by sorting the tail and merging it with the base, and keeps the table in memory as document 14 section 14.6 keeps other tables for each code.

**Codes are byte exact.** Two strings with different bytes have different codes. Under a deterministic collation, which is every collation that PostgreSQL ships by default, equality is byte equality, so group by and join use codes directly. Under a nondeterministic ICU collation, two different codes can be equal. For those columns the executor builds a table from code to equivalence class once for each dictionary version and collation, and groups and joins on the class. The base is sorted in the column's collation. If a query uses another collation, for example `ORDER BY url COLLATE "C"` on a column with an ICU collation, the rank table is built for that collation.

### 9.5.6 Shards, partitions and shared tables

**Each shard has its own dictionaries.** A code is local to its shard. A distributed query that groups on a string either sends strings between shards or sends a dictionary translation for the codes it uses. Document 18 decides which.

**Two columns do not share a dictionary.** rudb tested shared dictionaries across the string columns of `hits` and found that they saved 4 KB, because only `UTMSource` and `UTMCampaign` overlapped among 5,460 pairs of columns (`02-the-goal` section 2.6.1). rudb also tested rules between columns, and only `ClientIP` to `IPNetworkID` held, saving 18 MB. `URL` to `URLHash` failed on 11,586,966 rows. rupg has neither shared dictionaries nor rules between columns.

**A symbol table is shared by the dictionary and the `FSST` segments of one column.** It is trained once (section 9.4). When the ratio of new data falls below 0.8 times the ratio on the training sample, the next rewrite trains a new table.

Partitions of a partitioned table are separate tables, each with its own dictionaries. Document 10 section 10.12 gives the consequence for queries over the parent.

## 9.6 The compressed execution contract

An encoding in rupg-encode is more than a decoder. Each encoding implements a contract that the executor uses to avoid decoding. The contract is a Rust trait for each encoding with these operations.

| Operation | Meaning |
|---|---|
| `decode_into` | write the 1024 values of a vector into a flat array |
| `gather` | decode the values at given positions of a vector |
| `filter_eq`, `filter_range`, `filter_in` | produce a selection bitmap for a vector from a constant |
| `min_max` | the bounds of a vector, from metadata if possible |
| `sum`, `count` | the sum and count of the selected values |
| `codes` | for `GCODE` and `DICT`, the codes of a vector without lookups |
| `runs` | for `RLE` and `CONST`, the runs of a vector |

An encoding that cannot do an operation better than decode plus the generic kernel uses the default, which decodes. This table gives what each encoding does natively.

| Encoding | Filter | Aggregate | Group key | Random access |
|---|---|---|---|---|
| `CONST` | one compare for the vector | one multiply | one group for the vector | free |
| `FOR+BITPACK` | the constant moves into the packed domain, then a packed compare | sum of packed values plus base times count | decode | one shift and mask |
| `DELTA` | decode | decode | decode | decode from the vector start |
| `RLE` | one compare for each run | value times run length | one group for each run | binary search on run ends |
| `DICT` | the constant becomes a code set | sum by code counts | the code | one lookup |
| `GCODE` | the constant becomes a code or code range | not applicable | the code, section 9.5 | one lookup |
| `FSST` | the constant is compressed, then compared on compressed bytes for equality | not applicable | decode | one string |
| `ALP` | the constant moves into the integer domain where it is exact | decode, because float sums must add decoded values | decode | one value plus exception check |
| `ZSTD` | decode | decode | decode | decode the vector |

**`RLE` arithmetic carries the flag columns.** `hits` has 48 `SMALLINT` flag columns that collapse under `RLE` then `BITPACK` (`../2140/03-baselines.md` section 3.5.1). A sum over such a column is a sum of value times run length, a few thousand operations for the table instead of 100 million additions.

**Random access must be cheap for columns that the point path reads.** A point query or an update of a cold row (document 10 section 10.5) reads one row from each column of a segment. `DELTA` must decode from the start of the vector, which is at most 1023 extra values. Every other encoding reads one value. Lance version 2.1 (2025) showed a format that needs 1 to 2 I/Os for each random access. rupg needs one 64 KiB block read for each column of the row, because the segment directory gives the offset of each vector.

**The contract has one implementation for each kernel path.** rupg-kernels has AVX-512, AVX2, NEON, WebAssembly 128-bit and portable code for `BITPACK`, `FOR`, `DELTA`, `RLE` runs, the packed compares and the code gathers. The test suite of document 21 runs every encoding on every path and requires identical results.

## 9.7 Segment summaries

A segment summary is stored with each segment and describes the values of one column in that segment. Summaries make H1 of document 02 section 2.4.4 possible: scalars over the whole table, and over a filter that a whole segment passes, read summaries and no values. They are the zone maps that prune vectors for every other query. Document 12 section 12.6 gives how the planner uses them. Document 12 section 12.8 gives the frequency synopses of H3, and document 12 section 12.9 the gram summaries of H4. Those are also stored per segment and follow the same rules, and a synopsis only narrows which rows a query reads, as section 9.7.5 states.

### 9.7.1 Content

For each column of each segment, the summary holds these fields.

| Field | Type | Note |
|---|---|---|
| row count | u32 | all rows of the segment, including deleted |
| null count | u32 | |
| min, max | typed bound | with a tag for i128, f64 or bytes, as rudb `storage-v3/05` |
| sum | i128, f64 or numeric | section 9.7.2 |
| special flags | u8 | has NaN, has positive infinity, has negative infinity |
| distinct codes | u32 | for `GCODE` columns, the number of codes present |
| code set | reference | for `GCODE` columns, section 9.7.3 |

For each vector of 1024 values, the summary holds the minimum, the maximum and the null count. For strings with a dictionary, the bounds of a vector are the lowest and highest code ranks of the sorted base and a flag that says whether the vector holds tail codes. For strings without a dictionary, the bounds are the first 8 bytes of the lowest and highest strings, with a flag that says whether each bound is exact or a truncated prefix.

rudb's per-page bounds in `storage-v3/05` cut Q40 from 22.2 ms to 7.3 ms at one million rows, which is the effect of the per-vector bounds here.

**The summaries of a table are stored together, by column.** Document 08 section 8.9.1 keeps a table's summaries in its summary arena. Inside it, the segment summaries are grouped by column, so the summaries of one column for all segments are one contiguous read. `hits` has 99,997,497 rows (ClickBench), so it has 382 full segments, each with 256 vectors. The vector bounds of one 64-bit column are then 382 times 256 times 18 bytes before compression, 1.76 MB, and a cold query that filters on that column reads that much before it reads any value. The vector bounds are stored with `FOR` and `BITPACK`, because neighbouring vectors have close bounds.

### 9.7.2 Sums under PostgreSQL semantics

The result type of `sum` in PostgreSQL depends on the input type: `bigint` for `smallint` and `integer`, `numeric` for `bigint` and `numeric`, `real` for `real`, `double precision` for `double precision`, and the input type for `money` and `interval` (PostgreSQL 19 documentation, aggregate functions). `avg` of an integer type returns `numeric`. The summary sum must give the same result as a scan in each case.

**Integer sums are i128.** A segment of 262,144 rows of `bigint` has a sum of at most 2 to the power 81 in absolute value, which fits in 128 bits with room. The executor combines segment sums in i128 for `smallint` and `integer`, and in a 256-bit accumulator for `bigint`, then converts to the result type. If the result leaves the range of `bigint` for an input that PostgreSQL sums into `bigint`, the summary path raises the same error that the scan path raises, SQLSTATE `22003`.

**Exact numerics are scaled integers.** For a `numeric` column stored as scaled integers (section 9.8), the sum is an i128 at the column scale when it fits, and a `numeric` value when it does not. Each segment records which form it used. A NaN in the segment sets a flag and the sum is NaN, as in PostgreSQL. Positive and negative infinity in the same segment give NaN, as PostgreSQL gives for `numeric` since version 14.

**Float sums are stored in segment order.** A float sum depends on the order of the additions. The segment stores the f64 sum of its values added in row order. The table result adds segment sums, which is a different order from a serial scan. PostgreSQL also gives different last bits for different plans, for example a parallel plan against a serial one, and does not promise an order. rupg returns the sum of segment sums. Whether the compatibility suite accepts this is an open question in document 24.

**NaN orders above every number.** PostgreSQL orders NaN above all other `real` and `double precision` values. The summary's min and max follow that order. A segment with a NaN has max NaN, and the flag lets a query for `max` answer without a scan.

### 9.7.3 Code sets

A code set is the set of dictionary codes present in the non-null values of a column in one segment. `count(DISTINCT url)` over the table is the size of the union of the code sets of the segments, plus the hot rows. Code sets are stored in containers by code range: a sorted array of codes when the range has few codes, a bitmap when it has many, and runs when the codes are contiguous, which is the layout of Roaring bitmaps (Chambi, Lemire, Kaser and Godin, 2016).

**A code set is stored only when it is much smaller than the code column.** For `URL`, a segment can hold 262,144 distinct codes, and its code set can be as large as its code stream, which is 382 times up to 1 MiB for the table. A code set gives no saving over a scan of codes in that case. rupg stores a code set when its size is at most one eighth of the encoded code stream of the segment column. The fraction is a budget for M4.

### 9.7.4 Summaries under writes

Rule 5 of document 02 section 2.4.5 requires that a summary is maintained under `INSERT`, `UPDATE` and `DELETE` with the same guarantees as the data. Summaries are immutable, like the segment they describe. They are written in the transaction that publishes the segment. Deletes change no summary. The reader handles them.

| Case at the reader's snapshot | `count`, `sum` | `min`, `max` | `count(DISTINCT)` |
|---|---|---|---|
| no visible delete in the segment | summary | summary | code set |
| visible deletes | summary minus the deleted rows, read by `gather` | summary bound is a bound only, check by reading the deleted rows: if none holds the bound, the bound stands, else scan the segment | scan the code stream of the segment |
| segment not visible | skip | skip | skip |

Section 10.4 of document 10 gives the delete marks that the reader checks. The cost of a delete is therefore paid by the readers of that segment until compaction rewrites it. Compaction rewrites segments whose deleted share passes the threshold of document 10 section 10.9.

### 9.7.5 What a summary may answer

A summary answers a scalar that it holds exactly, for the rows that the snapshot sees: `count`, `sum`, `min`, `max` and `count(DISTINCT)` as section 9.7.4 allows. A summary never answers a grouped query. A group by computes its groups, counts and order at run time from rows, and a summary may only narrow which segments and vectors it reads. This is the rule of rudb `storage-v3/34`, which allows only reusable metadata and rejects any stored answer. It also forbids using a complete synopsis of a low-cardinality column as the output of a group by. The frequency synopses of H3 follow it: they prune the rows that cannot change the top k, and the query counts the rest. Document 02 section 2.4.4 states H3 as giving an answer "without a scan" when the bounds certify the top 10. That wording conflicts with this rule, and document 24 records the conflict.

**Deleting a summary changes no answer.** Every summary path has a scan path. Document 21 section 21.8 runs every suite with all summaries deleted, which is rule 4 of document 02 section 2.4.5.

## 9.8 PostgreSQL types to physical types

Each PostgreSQL type maps to a physical type. The physical type decides the candidate trees of section 9.3.1. The internal forms of PostgreSQL in the right column come from the PostgreSQL 19 documentation, chapter 8, and from the PostgreSQL headers for dates and times.

| PostgreSQL type | Physical type | PostgreSQL internal form |
|---|---|---|
| `boolean` | bool | 1 byte |
| `smallint`, `integer`, `bigint` | i16, i32, i64 | same |
| `oid`, `regclass` and other reg types | u32 | 4 bytes |
| `real`, `double precision` | f32, f64 | same |
| `numeric(p, s)` with p at most 18 | i64 scaled by 10 to the power s, with a display scale stream | base 10000 digits |
| `numeric(p, s)` with p at most 38 | i128 scaled, with a display scale stream | base 10000 digits |
| `numeric` without precision | i128 scaled when a segment fits, else opaque bytes | base 10000 digits |
| `money` | i64 | 8 bytes, cents |
| `date` | i32, days since 1 January 2000 | same |
| `time`, `timestamp`, `timestamptz` | i64, microseconds since 1 January 2000 for timestamps | same |
| `timetz` | struct of i64 and i32 | 12 bytes |
| `interval` | struct of i64 microseconds, i32 days, i32 months | 16 bytes |
| `uuid` | struct of two u64 | 16 bytes |
| `text`, `varchar`, `char(n)`, `name` | string | varlena, `char(n)` space padded |
| `bytea` | bytes | varlena |
| `enum` types | u32, the enum value OID | 4 bytes |
| `jsonb` | bytes, the PostgreSQL `jsonb` binary form | varlena |
| `json`, `xml` | string | varlena |
| arrays | list: offsets and a child column of the element type | varlena |
| composite types | struct: one child column for each attribute | varlena |
| `point`, `box` and other geometric types | struct of f64 children | fixed |
| `inet`, `cidr`, `macaddr` | bytes | varlena or fixed |
| `tsvector`, ranges, other types | opaque bytes in the type's binary send form | varlena |
| domains | the base type | the base type |

**The display scale of `numeric` is kept.** PostgreSQL keeps the display scale of each `numeric` value, so `1.5` and `1.50` print differently. The scaled form stores the scale of each value in its own stream, which is constant for most columns and costs one `CONST` vector header.

**`char(n)` keeps its padding.** The stored value has its trailing spaces, as in PostgreSQL. `RLE` and `FSST` remove most of the cost.

**The round trip is exact.** For every type, a value that goes into the cold store and comes out has the same binary send form as the value that went in. The test suite of document 21 checks this with generated values for every type, including NaN payloads, negative zero, infinities, the extreme dates and the longest strings.

## 9.9 TOAST thresholds

TOAST applies only to the hot store. Document 10 section 10.10 gives its structures. This section gives its thresholds and its compression.

**The thresholds are those of PostgreSQL.** A row of the hot store is a candidate for TOAST when it is larger than 2,040 bytes, the default of the storage parameter `toast_tuple_target` (PostgreSQL 19 documentation, `CREATE TABLE`). rupg compresses the largest values first, then moves values out of line until the row fits below the target. The storage parameter `toast_tuple_target` and the column storage modes `PLAIN`, `EXTERNAL`, `EXTENDED` and `MAIN` work as in PostgreSQL. A rupg page is 16 KiB, not 8 KiB, so rupg could keep larger rows inline. It does not, because the PAX hot pages of document 10 section 10.3 hold more rows when rows are small, and because clients that read `pg_column_size` expect PostgreSQL's behaviour.

**The compression is lz4 by default.** PostgreSQL 19 made `lz4` the default of `default_toast_compression`. rupg uses the same default and supports `pglz` when a column or the setting asks for it. `pg_column_compression` returns the method of a compressed hot value. For a value in the cold store it returns NULL, because the value is not compressed as a single datum. Whether NULL is the right answer is an open question in document 24.

**The cold store has no TOAST.** When the mover moves a row with TOAST values, it reads them whole and encodes them with the rest of the column. A value larger than 1 MiB is stored in its own extent with `ZSTD`, and the column holds a reference. 1 MiB is a budget.

## 9.10 Gates

| Gate | Number | Milestone | Source |
|---|---|---|---|
| G3 | `hits` below 9.45 GB including summaries | M4 | document 02 section 2.10 |
| G8 | `hits` below 7.67 GB including summaries and projections | M8 | document 02 section 2.10 |
| load | at most 1.25 times the read time of `hits.parquet` | M4 | document 02 section 2.4.8, document 20 section 20.3 |
| cold bytes | at most 2.78 GiB read over the 43 queries | M8, with G7 | document 02 section 2.4.7 |
| round trip | every type and every encoding exact on every kernel path | M4 | section 9.8 |
| determinism | the same input gives the same bytes on x86-64 and aarch64 | M4 | section 9.3.3 |

The size is measured with `rupg inspect FILE space` (document 08 section 8.14) and with the file length, and the run reports both. The file length is the number that document 20 compares.

**G3 is not yet met by a measured design.** rudb stored `hits` in 9.65 GB, which is 0.2 GB above the 9.45 GB of G3. G3 therefore needs 0.2 GB of savings plus the bytes of the summaries. The table gives the plan.

| Item | Bytes | Status |
|---|---|---|
| rudb encoded `hits` | 9.65 GB | measured, `03-baselines` section 3.5.1 |
| of which `URL`, `Referer`, `OriginalURL` | 4.44 GB | measured |
| OptFSST training on the string columns | 7.3 percent less for the FSST part | measured on other data, ADMS 2026 |
| sort order chosen at load, document 10 section 10.7 | better runs in low-cardinality columns | not measured on `hits` |
| summaries and zone maps | plus 0.1 to 0.3 GB | budget from section 9.7.1 |
| projections at M8 | plus up to 5 percent of the table | budget from document 10 section 10.7 |

The plan for G3 does not yet have a measured saving that covers it. If the first M4 run misses G3, the report gives the bytes for each column and each structure from the `space` command, and the change that follows targets the largest item. The target does not move, as document 02 section 2.10 requires.
