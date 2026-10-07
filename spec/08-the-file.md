# 8. The file

Written 7 October 2026.

This document specifies the `.rupg` file. One database is one file, as document 00 and document 04 section 4.1 state. The log, the catalog, the row pages, the column segments, the indexes, the free space map and the temporary spill all live inside it. This document gives the byte layout, the header slots, the page and extent formats, the page table, the order of writes at a checkpoint, the log ring region, free space, the buffer manager, the checks against damage, encryption, the owner lock, the entry points of recovery, the inspect tool, the rules for format versions, and the crash tests that gate milestone M1. Document 09 specifies how column data is encoded inside extents. Document 10 specifies what the hot store and the cold store put in pages and extents. Document 11 specifies the log records and the recovery algorithm.

## 8.1 What the file must do

The file has seven requirements. Each one comes from a decision in documents 00, 02 or 04, and each one constrains the layout below.

| Requirement | Source | Consequence for the layout |
|---|---|---|
| One file at rest and in operation | document 00, document 04 section 4.1 | the log rings, temporary spill and lock region are regions of the file |
| A committed transaction survives a crash | document 02 section 2.7.4 | two header slots, out-of-place page writes, per-worker log rings |
| Cold queries read at most 2.78 GiB over 43 queries | document 02 section 2.4.7 | summaries of a table sit in contiguous extents, read units are 64 KiB |
| Disk below 7.67 GB for `hits` at M8 | document 02 section 2.6.3 | fixed overhead per page and per extent is small and counted |
| The same file opens on Linux, macOS, Windows and in WebAssembly | document 04 section 4.10 | little-endian byte order, no `mmap` of the file, a fallback buffer manager |
| One owner process, others connect to it | document 00 | a lock region with the socket address of the owner |
| Every layer has a text form | document 04 section 4.8 | the inspect tool prints every structure in this document as text |

**The file is not mapped into memory.** The buffer manager reserves anonymous virtual memory and reads pages into it with explicit I/O (section 8.8). The file itself is never the target of `mmap`. A mapped file gives the kernel control over write-back order, and section 8.6 needs that order to be under rupg control. The same reason made LMDB depend on the ordering behaviour of the host file system, which rupg does not want to depend on.

## 8.2 The byte layout

All integers in the file are little endian. All floating point values are IEEE 754 binary32 or binary64, little endian. No tier 1 or tier 2 platform of document 04 section 4.10 is big endian. A big-endian host must swap bytes on read and on write, and no big-endian host is planned.

The file is a sequence of 16 KiB pages. Page number `n` starts at byte offset `n * 16384`. The first three pages have fixed roles. Every other page belongs to an arena (section 8.9).

| Page | Bytes | Role | Written |
|---|---|---|---|
| 0 | 0 to 4095 | identity block | at create and at format upgrade |
| 0 | 4096 to 8191 | owner record (section 8.12) | each time an owner opens the file |
| 0 | 8192 to 16383 | reserved, zero | never |
| 1 | 0 to 4095 | header slot A | at alternate checkpoints |
| 2 | 0 to 4095 | header slot B | at alternate checkpoints |
| 3 and up | all | arenas | out of place |

**The two header slots are in different pages.** LMDB keeps two meta pages for the same reason (Chu, LMDB design notes). A torn write to one slot must not damage the other one. Two slots in separate 16 KiB pages cost 32 KiB per file, which is not measurable against the disk targets of document 02 section 2.6.3. Each slot write is a single 4 KiB write at the start of its page, aligned for direct I/O.

### 8.2.1 The identity block

The identity block lets a tool recognise the file and read the page size before it reads a slot. The header slots repeat its important fields, and the slots are authoritative. A damaged identity block therefore does not lose the database.

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 8 | magic | bytes `89 52 55 50 47 0D 0A 1A` (`\x89RUPG\r\n\x1a`) |
| 8 | 2 | format major | 0 until the first release, then 1 |
| 10 | 2 | format minor | starts at 0 |
| 12 | 1 | page size shift | 14, for 16 KiB |
| 13 | 1 | checksum algorithm | 1, the four-lane checksum of section 8.3.2 |
| 14 | 2 | reserved | zero |
| 16 | 8 | compatible feature flags | section 8.15 |
| 24 | 8 | read-only-compatible feature flags | section 8.15 |
| 32 | 8 | incompatible feature flags | section 8.15 |
| 40 | 16 | file id | 128 random bits from the operating system at create |
| 56 | 8 | create timestamp | HLC value, document 04 section 4.7 |
| 64 | 32 | creator version | UTF-8, zero padded, for example `rupg 0.4.0` |
| 96 | 128 | encryption parameters | zero when not encrypted, section 8.11 |
| 224 | 3864 | reserved | zero |
| 4088 | 8 | checksum | the four-lane checksum of bytes 0 to 4087 |

**The magic follows the PNG pattern.** The PNG signature (RFC 2083 section 12.11) starts with a byte above 127 and contains a CR LF pair and a Ctrl Z byte. A transfer that strips the high bit, converts line endings or stops at end of file in text mode changes the magic. The open path then refuses the file with a clear message, not with a checksum error deep inside the page table.

### 8.2.2 The header slots

A header slot names the state of the database as of one checkpoint. It holds the roots of every structure that is not reachable from another structure.

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | slot magic, `RUPGSLOT` |
| 8 | 8 | generation, increases by 1 at each checkpoint |
| 16 | 16 | file id, equal to the identity block |
| 32 | 24 | the three feature flag words |
| 56 | 8 | checkpoint timestamp, an HLC value |
| 64 | 8 | file length in pages |
| 72 | 16 | page table root: physical page number and its checksum |
| 88 | 16 | catalog root: logical page number and its checksum |
| 104 | 16 | log ring directory: physical page number and its checksum |
| 120 | 16 | free space map root: physical page number and its checksum |
| 136 | 16 | shard map root: logical page number and its checksum |
| 152 | 16 | pending free list: physical page number and its checksum |
| 168 | 8 | next logical page number |
| 176 | 32 | key check value, zero when not encrypted |
| 208 | 8 | shutdown state: 1 when this checkpoint ended a clean shutdown, else 0 |
| 216 | 3872 | reserved, zero |
| 4088 | 8 | checksum of bytes 0 to 4087 |

**The open path takes the valid slot with the higher generation.** A slot is valid when its magic is correct, its checksum is correct and its file id equals the identity block or the other valid slot. If both slots are valid, the higher generation wins. If one slot is valid, it wins. If no slot is valid, the open fails with SQLSTATE `XX001` and names `rupg inspect --salvage` (section 8.14). This is the rule of rudb `storage-v3/02`, where the highest valid generation of two footer slots wins.

**A root carries the checksum of the page it names.** The open path checks the root page against the checksum in the slot before it uses the page. A slot that names a page that was never written, because of a lost write, is then detected at open, not at the first query.

## 8.3 The page

Row pages, hot store tree pages, index pages, catalog pages, page table pages and free space map pages are 16 KiB pages. A 16 KiB page is the size of document 04 section 4.7. PostgreSQL uses 8 KiB. A larger page halves the number of page table entries and buffer manager state words for the same data, and it holds more rows of a wide table in one PAX page (document 10). The cost is more bytes written for a small change, and section 8.6 reduces that cost with out-of-place writes.

### 8.3.1 The page header

Every 16 KiB page starts with a 64-byte header.

| Offset | Size | Field | Meaning |
|---|---|---|---|
| 0 | 8 | checksum | four-lane checksum of bytes 8 to 16383 |
| 8 | 8 | page timestamp | HLC value of the newest log record applied to the page |
| 16 | 8 | logical page number | the page's own identity, section 8.5 |
| 24 | 4 | table OID | owner object, 0 for system structures |
| 28 | 2 | shard | shard number, document 04 section 4.7 |
| 30 | 1 | kind | table below |
| 31 | 1 | kind version | layout version of this kind |
| 32 | 2 | flags | bit 0 encrypted, bit 1 unlogged, bit 2 temporary |
| 34 | 2 | lower | end of the slot area, for kinds with slots |
| 36 | 2 | upper | start of the heap area, for kinds with a heap |
| 38 | 2 | reserved | zero |
| 40 | 24 | kind data | sibling links, tree level and other fields of the kind |

**The page timestamp is an HLC value, not a log offset.** rupg has one log ring per worker (section 8.7), so there is no single log offset to compare against. The page timestamp is taken from the same hybrid logical clock as commit timestamps. No uncommitted version reaches the disk, because the page writer rolls back uncommitted versions in its copy of the page (document 11). The page timestamp of a written page is the newest visible commit timestamp at the time of the copy. Because the HLC is a Lamport clock across all rings, it orders the changes to one page across rings, which is the role that a global sequence number has in the LeanStore logging design (Haubenschild, Sauer, Neumann and Leis, SIGMOD 2020). Document 11 uses this field to decide which records recovery applies to a page.

**The logical page number detects a misdirected write.** The page is read from a physical location that the page table gives. If the page there names a different logical page, the read fails with `XX001`. A checksum alone does not detect this case, because the misdirected page has a correct checksum for its own content.

**A page that has no logical number holds its physical number.** The page table cannot map its own nodes, and the free space map is named by physical numbers. A page table node and a free space map page therefore hold their physical page number in the logical page number field, and the same check applies.

The page kinds are these.

| Kind | Name | Owner crate |
|---|---|---|
| 1 | page table node | rupg-file |
| 2 | free space map | rupg-file |
| 3 | log ring directory | rupg-log |
| 4 | catalog tree | rupg-catalog |
| 5 | hot store inner node | rupg-hot |
| 6 | hot store leaf, PAX | rupg-hot |
| 7 | undo spill | rupg-txn |
| 8 | delete mark pages | rupg-column |
| 9 | segment directory | rupg-column |
| 10 | TOAST chunk | rupg-table |
| 11 | B-tree node | rupg-index |
| 12 | hash, GIN, GiST, SP-GiST and BRIN nodes | rupg-index |
| 13 | graph adjacency | rupg-graph |
| 14 | vector graph | rupg-ann |
| 15 | temporary spill | rupg-exec |
| 16 | shard map | rupg-cluster |

Extents (section 8.4) have no page headers. The segment directory describes them.

### 8.3.2 The checksum

The checksum is the 64-bit four-lane function of rudb `storage-v3/07`, version 5. It keeps four independent 64-bit lanes, consumes 32 bytes per iteration, and ends with an avalanche step that mixes the lanes. Four independent lanes let the processor overlap the multiplies of one iteration, so the cost per byte is below a serial hash. The function, its constants and its test vectors are fixed at M1 and live in rupg-file, because rupg-file is below rupg-kernels in the layer order of document 22 and every layer above it can call it. The function gives the same result as XXH64 with seed 0, so an outside tool can check a page. A change to the function is a new algorithm number in the identity block and an incompatible feature flag.

**The checksum is always on.** PostgreSQL 18 turned data checksums on by default in `initdb`. rupg has no setting to turn them off. The read-only setting `data_checksums` shows `on`. The F3 format (SIGMOD 2025) measured that a checksum for each I/O unit is almost free relative to the cost of decoding, so the saving from turning checksums off does not justify the risk.

## 8.4 Extents

Column data lives in extents. An extent is a run of contiguous pages that one segment column, one dictionary or one summary group owns. Extents are immutable after the transaction that publishes them commits.

| Class | Size | Pages | Typical content |
|---|---|---|---|
| E0 | 64 KiB | 4 | a small column of a small segment, a summary group of a small table |
| E1 | 256 KiB | 16 | a column of 262,144 values bit-packed at 8 bits |
| E2 | 1 MiB | 64 | a column of 262,144 32-bit values, dictionary codes of a wide dictionary |
| E3 | 4 MiB | 256 | string payloads, dictionary payloads |
| E4 | 16 MiB | 1024 | large dictionaries, log ring segments, bulk string payloads |

The classes follow rudb `05-storage`, which used block size classes from 64 KiB to 16 MiB. A column of one segment that needs more than 16 MiB uses a list of E4 extents. A column that needs less than 64 KiB shares an E0 extent with other small columns of the same segment, in a packed extent that the segment directory describes by offset and length.

**The unit of checksum and of read is 64 KiB.** The segment directory stores one checksum for each 64 KiB block of each extent. A reader that needs one vector reads only the 64 KiB blocks that hold it, checks them, and decodes. rudb found that striping column data into 1,024-row chunks gave about 981 page reads for one million rows (`storage-v3/09`), so the read unit must be larger than one vector of one column. A 64 KiB block holds 1024 values at up to 512 bits each, which covers every fixed-width encoding of document 09. The unit also stays below the 256 KiB that AWS counts as one I/O on a gp2 volume (AWS EBS documentation, I/O characteristics), so a scan of a whole extent costs one I/O per 256 KiB and a probe costs one I/O.

**Extents are aligned to their size.** An extent of class E2 starts at a page whose offset from the start of its arena is a multiple of 64. The first arena starts at page 3, so the alignment is to the arena and not to page 0. The allocator is a buddy allocator inside 16 MiB arenas (section 8.9). Alignment lets the allocator merge free neighbours with one bit test, and it keeps every extent inside one arena.

DuckDB uses one block size of 256 KB with an 8-byte checksum for each block (DuckDB storage documentation). One size wastes space for small columns and splits large columns into many blocks. Size classes avoid both costs at the price of a buddy allocator.

## 8.5 The page table

A pointer from one page to another, for example from a B+tree inner node to a child, holds a logical page number. The page table maps a logical page number to the physical page number where the newest durable or written copy of the page is. This indirection is what allows out-of-place writes (section 8.6): a page moves without a change to the pages that point to it.

**Only 16 KiB pages go through the page table.** Extents are immutable, so they never move during normal operation. The segment directory names extents by physical page number. Compaction and file shrink (section 8.9) move an extent by writing a new segment directory entry in a transaction.

**The page table is a radix tree of 16 KiB pages.** Each node holds 2,040 entries of 8 bytes after its 64-byte header. Three levels address 2,040 to the power 3, about 8.49 billion logical pages, which is 126 TiB of 16 KiB pages. A fourth level is added when the next logical page number passes that limit. The root's physical location is in the header slot.

| Bits | Field |
|---|---|
| 0 to 47 | physical page number, 0 means not allocated |
| 48 to 63 | write tag: the low 16 bits of the page timestamp of the written copy |

**The write tag detects a lost write.** When a write to a new location is lost, the location still holds whatever was there before, and that can be an older copy of the same logical page, with a correct checksum and the correct logical page number. The write tag in the entry then differs from the page timestamp in the page, and the read fails with `XX001`. A 16-bit tag misses a lost write with probability 1 in 65,536 when the old copy is of the same logical page. The full page timestamp would need 16 bytes per entry, and that doubles the page table.

The page table of a database with 10 GiB of 16 KiB pages has 655,360 entries, which is 5 MiB. Its pages are ordinary pages in the buffer pool, and the buffer manager keeps them resident while they are dirty. For a file whose hot part is below 1 TiB, the page table is at most 512 MiB and normally stays resident. These are calculations from the entry size, not measurements.

Logical page numbers are reused. When a page is freed, its logical number goes to a free list in the page table, and the next allocation takes it after the checkpoint that frees it is durable (section 8.6).

## 8.6 Out-of-place page writes and the checkpoint

A 16 KiB page is never written over its own durable copy. The page writer writes it to a free physical location and updates the page table entry in memory. The durable page table, which a header slot names, still points to the old copy until the next checkpoint. A crash at any moment therefore leaves a consistent tree of pages reachable from the newest valid slot, and the log brings that tree forward.

**Out-of-place writes are faster and wear the flash less.** "How to Write to SSDs" (PVLDB 19, 2026) measured that an in-place 4 KiB update costs 18.85 KiB of flash writes inside the device. With out-of-place writes, the same work ran at 1.65 to 2.24 times the throughput with 6.2 to 9.8 times fewer flash writes on YCSB A, and at 2.45 times the throughput on TPC-C. The reason is that the device sees large sequential writes into free regions instead of small random overwrites. rupg takes this design because the transactional target of document 02 section 2.7 needs the throughput and the embedded use needs the wear behaviour on laptops and phones.

**There are no full page images in the log.** PostgreSQL writes a full page image to the WAL at the first change of a page after a checkpoint, so that a torn page write can be repaired (PostgreSQL documentation, `full_page_writes`). rupg never overwrites the durable copy, so a torn write damages only a copy that nothing durable points to. The log stays small, which matters for the per-worker rings of section 8.7.

### 8.6.1 The page writer

The page writer is a maintenance task in queue 3 of document 04 section 4.5. It picks dirty pages from the buffer pool, oldest first dirty position first, and it writes them in batches.

1. It takes the dirty page under a shared latch and copies it into a write buffer. The copy lets writers change the page again while the I/O runs.
2. It checks the log rule. Every log record that changed the page must be durable before the page is written. The page timestamp names the newest change. The writer waits until each ring that holds a change to the page is durable at or above that timestamp. Document 11 gives the record of which rings changed a page.
3. It computes the checksum, and it encrypts the page if the file is encrypted (section 8.11).
4. It takes a physical location from the current write arena. Locations inside one write arena are given in increasing order, so one batch becomes one or a few large sequential writes.
5. It submits the writes. On Linux it uses `io_uring` with the file opened with `O_DIRECT`. On macOS it uses `pwrite` from the I/O thread pool with `F_NOCACHE` set on the file.
6. When the write completes, it sets the page table entry to the new location and write tag, and it marks the page table node dirty. It puts the old physical location on the pending free list of the current checkpoint interval.
7. If the page did not change during the write, it marks the page clean.

The old location stays allocated after step 6. The durable page table may still point to it, and a crash before the next checkpoint must find it intact.

### 8.6.2 The checkpoint

A checkpoint makes the current page table, catalog root and free space map reachable from a header slot. It is incremental, as in LeanStore, where an incremental checkpoint writes about 1 percent of the buffer pool for each GB of log (Leis and others, LeanStore). A checkpoint does not write every dirty page. It writes the pages that hold the oldest log positions, so that the log can be truncated, and it leaves the rest dirty.

These are the steps, in this order. A step starts only when the previous step is complete.

1. **Fix the redo positions.** For each ring, the checkpoint computes the oldest log position that changed a page that is dirty in memory and has not been written. That is the position where redo of the ring must start after this checkpoint.
2. **Write the old dirty pages.** The page writer writes the dirty pages whose first change is older than the checkpoint target, as in section 8.6.1, and waits for every write to complete. The target is the position that leaves at most `max_wal_size` of log in each ring.
3. **Write the metadata pages.** The checkpoint writes, out of place, the dirty page table nodes from the leaves to the root, the dirty free space map pages, the pending free list pages, and a new copy of the log ring directory with the redo positions of step 1. It waits for every write to complete.
4. **Sync the data.** It calls `fdatasync` on the file. On macOS it calls `fcntl` with `F_FULLFSYNC`, because `fsync` on macOS does not flush the drive's write cache (Apple `fsync(2)` manual page).
5. **Write the slot.** It writes the inactive header slot with the generation of the active slot plus 1, the new roots and their checksums, and the checkpoint timestamp. The write is one 4 KiB direct write.
6. **Sync the slot.** It calls `fdatasync`, or `F_FULLFSYNC` on macOS.
7. **Release.** The physical pages on the pending free list of the previous interval become free. The ring space before the new redo positions becomes free. The new slot is now the active slot.

**The invariant is that the durable slot never points to a location that can be overwritten.** A location becomes free only in step 7, after the slot that no longer refers to it is durable. Every crash case follows from this invariant.

| Crash during | Slot found at open | State at open |
|---|---|---|
| steps 1 to 4 | the old slot | old page table, old pages intact, redo from the old positions |
| step 5 | the old slot, because the new one fails its checksum | as above |
| step 6 | either slot, both are complete | consistent in both cases |
| step 7 | the new slot | redo from the new positions, pending frees are recomputed |

**The checkpoint triggers are the PostgreSQL settings.** A checkpoint starts when `checkpoint_timeout` passes, default 5 minutes, or when any ring holds more than `max_wal_size` of log since its redo position, default 1 GB, as in PostgreSQL. The SQL command `CHECKPOINT` runs one and waits for it. `checkpoint_completion_target` paces step 2. These settings exist in PostgreSQL and clients read and set them, so rupg keeps their names and meanings (document 19).

### 8.6.3 Segments and the log

Extents of a new segment are written by the mover or by a bulk load (document 10 sections 10.6 and 10.8), not by the page writer. Their content is not in the log. The order is this.

1. The writer allocates extents, writes them with direct I/O, and waits for completion.
2. It calls `fdatasync`, or writes the last extent with `RWF_DSYNC`.
3. The publishing transaction writes a log record that names the segment, its extents and the checksums of their 64 KiB blocks, and then commits.

A crash before step 3 is durable leaves extents that no durable commit names. Recovery frees them (section 8.13). A crash after step 3 finds the extents durable and correct, because step 2 happened first. The cost is one extra sync for each publication, which the mover amortizes over a segment of up to 262,144 rows.

## 8.7 The log ring region

Each worker has one log ring inside the file (document 04 section 4.3). A ring is a list of E4 extents of 16 MiB, written as a circular buffer. A ring starts with four extents, 64 MiB. Document 11 specifies the records, the commit dependencies between rings, group commit and the redo order. This section specifies only the space.

**The ring directory is one 16 KiB page.** The header slot names it. It has one entry of 96 bytes for each ring, so one page holds 168 rings. A node with more workers uses a chain of directory pages.

| Size | Field |
|---|---|
| 4 | ring number |
| 4 | worker number at the time of the checkpoint |
| 8 | ring size in bytes |
| 8 | redo position: the ring offset where redo starts |
| 8 | durable position at the checkpoint |
| 8 | newest durable HLC value at the checkpoint |
| 8 | physical page number of the extent list |
| 8 | checksum of the extent list page |
| 40 | reserved |

**A ring position is a 64-bit byte offset that only increases.** The physical location is the offset modulo the ring size, mapped through the extent list. Every record header holds its own position and a checksum. Recovery stops reading a ring at the first record whose position or checksum is wrong, which is how it finds the end of a ring that wrapped.

**A flush never rewrites a block that holds a durable record.** The ring writes in units of 4 KiB, the alignment of direct I/O in section 8.2. A flush pads the last unit with a fill record. The next flush starts at the next unit. If the ring rewrote a partial unit, a torn write of that unit could destroy records that were already reported as committed. The padding costs space, and per-worker group commit amortizes it. "Moving on From Group Commit" (SIGMOD 2025) is the reference that document 11 uses for the commit protocol.

**A ring grows when the checkpoint cannot keep up.** If a ring has no free space, its worker first starts a checkpoint. If the checkpoint does not free space because one transaction holds an old position, the ring adds an extent. The new extent is usable only after a checkpoint writes the new extent list and a slot that names it, so the growth costs one checkpoint. A ring shrinks back to four extents at a checkpoint when it has been less than one quarter full for 10 checkpoints. These numbers are budgets for M1, to be tuned against the TPC-C runs of M5.

## 8.8 The buffer manager

The buffer manager in rupg-buffer gives every layer access to pages and to extent blocks. It is one of the crates that may contain `unsafe` (document 04 section 4.8). It follows vmcache (Leis, Alhomssi, Ziegler, Loeck and Dietrich, SIGMOD 2023), as document 04 section 4.6 decides.

### 8.8.1 The mapping

At open, the buffer manager reserves two ranges of anonymous virtual memory with `mmap`, `PROT_READ | PROT_WRITE` and `MAP_NORESERVE`. Neither range maps the file. A reservation costs no physical memory until a page is touched.

| Window | Indexed by | Unit | Content |
|---|---|---|---|
| page window | logical page number | 16 KiB | every 16 KiB page of section 8.3 |
| extent window | physical page number | 64 KiB block | extent blocks of section 8.4 |

The address of logical page `n` is the page window base plus `n * 16384`. A pointer to a page is therefore an addition, not a hash lookup. This is the property that makes vmcache faster than a hash table pool: vmcache measured that its optimistic reads cost less than 8 percent more than reads of plain memory.

**Each window reserves one quarter of the user address space.** On x86-64 with 4-level page tables the user address space is 128 TiB (Linux kernel documentation, x86-64 memory map), so each window is 32 TiB. On aarch64 kernels with 48-bit virtual addresses it is 64 TiB. Some aarch64 kernels use 39-bit addresses, which gives 512 GiB of user space and 128 GiB for each window. A file larger than its window on such a host opens with the fallback of section 8.8.4. At open the pool asks for 2^31 pages and halves the request until the operating system gives the range, down to 2^22 pages (64 GiB). A host that refuses 2^22 pages, or that has operating system pages larger than 16 KiB, uses the fallback.

**Each unit has a 64-bit state word.** The state words are in a separate array, also reserved with `MAP_NORESERVE`. A state word holds a version counter and a state: evicted, unlocked, locked shared with a count, or locked exclusive. This is the vmcache state word.

The high 8 bits of the word hold the state and the low 56 bits hold the version. State 0 is evicted, 1 is unlocked, 2 to 253 is locked shared with 1 to 252 readers and 254 is locked exclusive. A word of zero bytes is therefore an evicted unit with version 0. The operating system gives the state word array as zero bytes, so the array needs no setup at open and costs no memory for a unit that was never read. The release of an exclusive latch and an eviction increase the version. A shared latch does not change the version, so it does not make an optimistic read fail.

### 8.8.2 Reads, writes and eviction

**An optimistic read takes no lock.** The reader loads the state word, reads the page, and loads the state word again. If the version is the same and the page was not locked exclusive, the read is valid. Otherwise the reader retries. B+tree descents in the hot store and in indexes use optimistic reads at every inner level.

**A shared or exclusive latch changes the state word with one compare and swap.** A writer takes the exclusive latch, changes the page, increases the version and releases. The Rust API returns guard types for the three modes, and a guard releases its latch when it is dropped.

**A miss reads the unit into its own address.** When the state is evicted, the reader takes the exclusive latch, issues the read into the unit's address in the window, checks the checksum and the logical page number or the extent block checksum, and releases the latch as shared or unlocked. With `io_uring` the read is asynchronous and the session task yields until it completes.

**Eviction uses a clock and `madvise`.** The resident units are in a list that a clock hand sweeps. A unit that was used since the last sweep gets a second chance. A clean unit is evicted by setting its state to evicted and calling `madvise(MADV_DONTNEED)` on its address range, in batches of 64 units to amortize the system call. A dirty unit goes to the page writer first. vmcache measured that the exmap kernel module scales eviction further, but exmap is a kernel module that cloud images do not ship, and document 04 section 4.6 decides that rupg does not require it.

**One write of a page is in flight at a time.** Two threads can find the same dirty unit, for example the page writer and a thread that needs a free unit. If both copied and wrote the page, the older copy could complete last and replace the newer one. Each unit therefore has a flag that a thread sets before it copies the page and clears after the write completes. A second thread that finds the flag set does not write the page, and the unit stays dirty. A page is freed only when its flag is clear.

**An optimistic reader reads with volatile reads.** A writer can change the bytes while an optimistic reader reads them, and the reader discards what it read when the version changed. Rust has no atomic copy of bytes yet (RFC 3301), so rupg-buffer reads the bytes with volatile reads and never makes a Rust reference to them. The crossbeam seqlock uses the same method. Miri reports these reads as a data race, so the test that runs readers and writers on threads does not run under Miri.

### 8.8.3 Size and memory

The resident units are the buffer pool. Their size is part of the one budget `rupg.memory_limit`, default 25 percent of physical memory for the server and 256 MiB for the library (document 04 section 4.6). The pool never shrinks below `shared_buffers`. When an operator asks for memory and the budget has none, the pool evicts clean units and returns their bytes to the budget. When operators release memory, the pool can grow again.

**The pool grows in steps and gives back clean units only.** A miss that finds the pool at its size takes 64 more pages from the budget, so that a miss does not lock the budget each time. When another consumer needs memory, the budget calls the reclaimer of the pool, which evicts clean units and gives their pages back. A dirty unit keeps its memory until the page writer writes it. The reclaimer does not wait for a lock that the pool holds, so a miss that grows the pool and asks the budget for memory does not wait for itself.

**The page table of the process counts.** Each resident 4 KiB operating system page needs a page table entry in the kernel. For a pool of 8 GiB this is 2,097,152 entries of 8 bytes, 16 MiB. rupg reads its own resident set size from the operating system for the memory gate of document 02 section 2.6.2, so this cost is in the measured number.

**Prefetch is driven by the plan.** A scan that knows which vectors it needs, from zone maps and summaries (document 09 section 9.7), submits reads for the 64 KiB blocks ahead of the executor. Haas and Leis (PVLDB 16, 2023) showed that 12.5 million I/Os per second needs about 3,000 I/Os in flight. A worker therefore keeps up to 256 reads in flight on its own ring, which gives 4,096 in flight on a 16-core machine. 256 is a budget, to be measured at M4. The file descriptor is registered with `io_uring` as a fixed file. Buffers are not registered, because a registered buffer must be a fixed region and the windows are too large to register.

### 8.8.4 The fallback without virtual memory

WebAssembly has one linear memory. It cannot reserve address space without using it, and the memory can grow but not shrink (WebAssembly core specification, `memory.grow`). Some hosts also forbid large reservations, for example a process with a `ulimit -v` limit or a 39-bit aarch64 kernel with a large file.

On these hosts, rupg-buffer uses a hash table from unit number to frame. The frames are a fixed array, allocated at open from the memory budget. The state word and the guard API are the same. The only difference is the extra hash lookup on each access, which vmcache measured as the cost that its design removes. In WebAssembly, reads and writes go to the Origin Private File System through a `FileSystemSyncAccessHandle`, which gives synchronous `read`, `write`, `flush` and `truncate` and is available only in a dedicated worker (WHATWG File System standard). `flush` is the sync of steps 4 and 6 of section 8.6.2.

The F3 format measured that WebAssembly decoding is 15 to 46 percent slower than native. The fallback adds the hash lookup to that cost. WebAssembly is not a benchmark platform in document 20, so this cost is accepted.

## 8.9 Free space

### 8.9.1 Arenas

The file after page 2 is a sequence of arenas of 16 MiB, which is 1024 pages. Each arena has one kind.

| Arena kind | Allocation | Content |
|---|---|---|
| write arena | sequential | 16 KiB pages written by the page writer |
| extent arena | buddy, classes E0 to E4 | segment column extents, dictionary extents |
| summary arena | buddy | summary extents, one table's summaries kept together |
| ring arena | whole arena | one E4 extent of a log ring |
| spill arena | sequential, not durable | temporary spill and temporary tables |

**The free space map has one entry for each arena.** An entry is 8 bytes: the kind, the count of free pages, and the largest free buddy class. Each arena also has a bitmap of 1024 bits, 128 bytes, with one bit for each page. For a 10 GiB file this is 640 arenas and 85 KiB of map. The map pages are written out of place at each checkpoint.

| Bytes of the entry | Field |
|---|---|
| 0 | arena kind: 0 free, 1 write, 2 extent, 3 summary, 4 ring, 5 spill |
| 1 | largest free class, 0 for E0 to 4 for E4, 255 when no aligned 64 KiB run is free |
| 2 to 3 | count of free pages, 0 to 1024 |
| 4 to 7 | reserved, zero |

**A map page holds 120 arenas.** After the 64-byte page header, each arena has 136 bytes: the entry, then the bitmap. 120 records fill the page exactly. The kind data of the header holds the first arena at bytes 0 to 7, the count of records in use at bytes 8 and 9, and the next map page at bytes 16 to 23, in the format of a page table entry. The header slot names the first map page. A read checks that each entry matches its bitmap, so a damaged entry gives `XX001` and not a wrong allocation.

**Summaries are kept together.** Document 02 section 2.4.7 requires that the summaries of a table sit in contiguous extents, so that a cold query loads them with one sequential read. The allocator gives each table a summary arena, or a share of one for small tables, and allocates its summary extents in order inside it. When the summary arena of a table is full, compaction (document 10) rewrites the table's summaries into a new arena in column order. A cold query then reads the summaries of the columns it needs in one read per column.

### 8.9.2 Reuse rules

A physical page or extent goes through three states: allocated, pending free, and free. These rules apply.

1. A page that the page writer superseded is pending free until the checkpoint after the write is durable.
2. An extent of a segment that compaction replaced is pending free until no snapshot can see the old segment. It then waits for one more checkpoint.
3. A logical page number that a tree released is reused only after the checkpoint that records its release is durable.
4. A write arena with fewer than 25 percent of its pages in use is a candidate for cleaning. The page writer copies its live pages to the current write arena, out of place, and the arena becomes free after the next checkpoint. 25 percent is a budget for M1.
5. Spill arenas are freed when the query or session ends, and at every open.

### 8.9.3 Growth and shrink

**The file grows by whole arenas.** When no arena of the needed kind has space, the file grows by the larger of one arena and one eighth of its size, rounded to arenas. On Linux the growth uses `fallocate`. On macOS it uses `fcntl` with `F_PREALLOCATE`. Growth by a fraction of the size keeps the number of growth calls logarithmic in the final size. The new length is recorded at the next checkpoint. If the disk is full, the statement that needed the space fails with SQLSTATE `53100`, as document 04 section 4.8 requires.

**The file shrinks at a checkpoint.** When the arenas at the end of the file are free after step 7 of section 8.6.2, the checkpoint truncates the file to the last arena in use. It writes the new length in the next slot first. Truncation runs only after that slot is durable.

**`VACUUM FULL`, `CLUSTER` and `REPACK` move data toward the start.** PostgreSQL 19 adds `REPACK`, which does the work of `VACUUM FULL` and `CLUSTER` (PostgreSQL 19 release notes). rupg accepts all three. For a table, they rewrite its hot pages and segments into arenas with low numbers and free the old space. Out-of-place writes make this the normal write path with a different choice of location. Plain `VACUUM` does not move data across arenas. It cleans write arenas under rule 4 and compacts segments as document 10 specifies.

## 8.10 Torn writes and bit rot

**A torn page write is harmless.** Section 8.6 never writes over a durable copy, so a torn write damages a location that no durable structure names. If the crash comes before the next checkpoint, recovery reads the old copy and redoes the log. A torn slot fails its checksum, and the other slot wins.

**Every read from the file checks the checksum.** A page read checks the page checksum, the logical page number and the write tag. An extent block read checks the block checksum in the segment directory. A failed check retries the read once with a fresh I/O. If the retry fails and the node has a replica (document 18), the node fetches the page or block from the replica and rewrites it out of place. Otherwise the statement fails with SQLSTATE `XX001`, and the message names the file, the page or extent and the expected and actual checksums. The failure is counted in `pg_stat_database.checksum_failures` and `checksum_last_failure`, as PostgreSQL does.

**A checksum error never becomes a NULL.** rudb `storage-v3/09` set this rule, and it holds here. A decoder that cannot trust its input stops the statement. The PostgreSQL settings `ignore_checksum_failure` and `zero_damaged_pages` are accepted for compatibility. They are superuser settings in PostgreSQL. In rupg they have this effect: `ignore_checksum_failure` lets `rupg inspect` and `pg_dump` read past a damaged page and skip its rows with a warning, and `zero_damaged_pages` has no effect except a warning, because rupg does not write zero pages over data.

**A scrubber reads the whole file in idle time.** The scrubber is idle work in queue 4 of document 04 section 4.5. It reads every allocated page and extent block at a rate set by `rupg.scrub_rate`, default 10 MiB per second, and it checks it. A full pass over a 10 GiB file takes about 17 minutes at that rate. The rate is a budget that keeps the scrubber below 5 percent of a gp2 volume's 250 MiB per second.

## 8.11 Encryption

Encryption is chosen when the file is created, with `rupg.encryption = 'aes-256-gcm'` and a key source. It cannot be turned on later except by a rewrite with `rupg-cli copy`. An encrypted file sets an incompatible feature flag.

**The data key is wrapped.** At create, rupg draws a 256-bit data key from the operating system. It wraps the data key with a key encryption key and stores the wrapped key, the key derivation parameters and a key identifier in the identity block, and a key check value in each slot. The key encryption key comes either from a passphrase through Argon2id (RFC 9106) or from a key provider that the embedding program or the server configuration supplies. A new passphrase rewraps the data key and changes no page.

**Each page and each extent block is sealed with AES-256-GCM.** The unit of encryption is the unit of checksum: the 16 KiB page and the 64 KiB extent block. An encrypted page keeps its 64-byte header in clear text and authenticates it as associated data, so a page cannot be moved to another logical number without detection. The page ends with a 32-byte trailer: a 12-byte nonce, a 16-byte tag and 4 bytes of zero. For extent blocks, the nonce and the tag are stored with the block checksum in the segment directory. DuckDB uses AES-GCM with a 40-byte header for each block (DuckDB encryption documentation), which is the same cost class.

**Nonces come from a counter.** Out-of-place writes write a page to a new location with new content each time, so a nonce must never repeat under the same key. The nonce is a 32-bit writer identity followed by a 64-bit counter that the writer increases for every seal. The counter is recorded in each checkpoint slot, and at open the counter starts above the recorded value plus the largest count that one checkpoint interval can use. NIST SP 800-38D section 8 requires unique nonces for one key. A deterministic construction meets it without the limit of 2 to the power 32 that applies to random nonces.

**The log rings and the spill are encrypted too.** Each 4 KiB unit of a ring is sealed with its own nonce. Temporary spill uses a key that is drawn at open and never stored, so spill content cannot be read after the process ends.

**The checksum is computed over the ciphertext.** A reader checks the checksum first. A checksum failure is corruption, `XX001`. A tag failure with a correct checksum is a wrong key or tampering, and it fails with SQLSTATE `XX001` and a distinct message. Key rotation of the data key requires a rewrite of every page, and it is an open question in document 24.

## 8.12 The owner and the lock region

One process owns a file at a time. Document 17 specifies how other processes connect to the owner and what they can do. This section specifies the lock and the owner record.

**The owner holds an exclusive lock on the file.** On Linux it takes an open file description lock with `fcntl(F_OFD_SETLK)` on bytes 4096 to 8191 of page 0. On macOS, which does not have open file description locks, it takes `flock(LOCK_EX | LOCK_NB)` on the whole file. On Windows it takes `LockFileEx` on the same byte range. In WebAssembly, `createSyncAccessHandle` already gives one handle exclusive access to the file, so no extra lock exists. The lock is released by the kernel when the owner process ends, so a crash does not leave a stale lock.

**The owner record says how to reach the owner.** After it takes the lock, the owner writes this record at offset 4096 of page 0 and syncs it.

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | record magic, `RUPGOWNR` |
| 8 | 4 | owner process id |
| 12 | 4 | owner protocol version |
| 16 | 8 | owner start time, HLC value |
| 24 | 16 | file id |
| 40 | 32 | host identity, a hash of the machine id and boot id |
| 72 | 2 | socket address length |
| 74 | 256 | socket address, a Unix socket path or a Windows named pipe name |
| 330 | 3758 | reserved |
| 4088 | 8 | checksum |

The socket path is at most 104 bytes, the size of `sun_path` on macOS. Linux allows 108. The field is 256 bytes so that a Windows pipe name fits. When the path does not fit in 104 bytes, the owner uses a path under the runtime directory of the user and writes that path.

**The owner record is the one in-place write in the file.** It is advisory. A process that wants to open the file first tries the lock. If the lock succeeds, the record is stale, and the process becomes the owner and writes a new record. If the lock fails, the process reads the record, checks its checksum, its file id and its host identity, and connects to the socket. A torn or stale record makes the connection fail, and the process retries after a short delay, up to the timeout that document 17 sets.

**Network file systems are refused by default.** Byte-range and whole-file locks are not reliable on all network file systems, and SQLite documents corruption from this cause (SQLite documentation, "How To Corrupt An SQLite Database File"). At open, rupg checks the file system type with `statfs`. On NFS, SMB and FUSE file systems it refuses to open unless `rupg.allow_network_fs` is on.

## 8.13 Recovery entry points

Every open runs the same path. A clean shutdown makes the path short, because the last checkpoint leaves no log to redo.

1. Read page 0. Check the magic, the format version and the feature flags (section 8.15).
2. Take the owner lock (section 8.12). If another process owns the file, connect to it instead and stop here.
3. Read both header slots and choose one (section 8.2.2).
4. Load the page table root, the ring directory, the free space map and the catalog root, and check each one against the checksum in the slot.
5. Redo. Read each ring from its redo position to its end. Apply the records to pages in the order of document 11, using the page timestamp to skip records that a written page already has.
6. Drop the log blocks of transactions that have no durable commit record. No page on disk holds their changes, so no undo is needed (document 11).
7. Reconcile free space. An extent that is allocated in no durable structure and named by no durable commit is freed. This covers segments whose publication did not commit (section 8.6.3).
8. Clear spill arenas. If the slot does not record a clean shutdown, truncate unlogged tables (document 10 section 10.13).
9. Run a checkpoint, so that the next crash does not redo the same log.
10. Write the owner record and accept connections.

**The recovery time depends on the log since the checkpoint.** With the default `max_wal_size` of 1 GB for each ring, the worst case is 1 GB of log on each ring. Document 11 sets the target for redo throughput and gives the measured number at M1.

| Entry point | Who calls it | Effect |
|---|---|---|
| open | server and library | the full path above |
| open read-only | `rupg.read_only = on` | steps 1 to 4, then redo into memory only, no write to the file |
| `rupg inspect` | operator | steps 1, 3 and 4 without the lock, reads only |
| `rupg verify` | operator, tests | reads and checks every reachable page and extent block |
| `rupg inspect --salvage` | operator | chooses a slot by hand, or rebuilds roots from a scan of page headers |

**Salvage is a last resort.** `--salvage` scans every page header, groups pages by logical page number, keeps the copy with the newest page timestamp that passes its checksum, and writes a new file. It cannot recover changes that were only in the log and whose ring is damaged. It prints what it lost.

## 8.14 The inspect tool and text dumps

`rupg inspect` is a command of rupg-cli. It opens a file read-only, without the owner lock, and follows only the structures that the active slot reaches. It can therefore run against a file that a server owns. It sees the state of the last checkpoint, which is consistent.

| Command | Output |
|---|---|
| `rupg inspect FILE header` | identity block and both slots, with which slot is active and why |
| `rupg inspect FILE page N` | one page by logical number: header, checksum result and the kind's text form |
| `rupg inspect FILE physical N` | one page by physical number |
| `rupg inspect FILE pagetable` | logical to physical entries, with ranges |
| `rupg inspect FILE rings` | ring directory, extents, redo and durable positions |
| `rupg inspect FILE fsm` | arenas, kinds, free counts |
| `rupg inspect FILE segments TABLE` | segment map and segment directories of a table |
| `rupg inspect FILE summaries TABLE` | the summaries of document 09 section 9.7 |
| `rupg inspect FILE dict TABLE COLUMN` | the dictionary of document 09 section 9.5 |
| `rupg inspect FILE space` | bytes by table, by structure and by kind |

**The text form is stable and parses back.** Document 04 section 4.8 requires a text form for every layer. Each kind of page and each segment directory prints as text, one field on each line, sorted, with no addresses or times that change between runs unless the file content changed. rupg-file can parse the text back into a page. Tests build pages by hand in text and compare the text dump of a page after an operation against a stored file. The `space` command gives the numbers that the disk gates of document 09 section 9.10 and document 20 report, broken down so that a regression points to its structure.

## 8.15 Versions and upgrade

The identity block and each slot have a format major and minor number and three feature flag words, in the style of the ext4 superblock (Linux kernel documentation, ext4 disk layout).

| Flag word | A reader that does not know a set bit |
|---|---|
| compatible | opens the file normally |
| read-only-compatible | opens the file read-only |
| incompatible | refuses to open the file and names the feature |

Encryption, a new checksum algorithm and a new page header layout are incompatible features. A new optional summary kind is a read-only-compatible feature, because an old writer would not maintain it. A new statistics record that readers can ignore is a compatible feature.

**Format major 0 makes no promise.** Until the first release, a new rupg version can refuse an older file, and `rupg-cli copy` converts it. The identifiers of document 04 section 4.7 are fixed at M1, so most format changes before the release are local to one kind of page or extent.

**From format major 1, every release opens every earlier major 1 file.** An upgrade that adds a feature sets its flag at the next checkpoint after the first use of the feature. A file never gets a flag for a feature it does not use, so a file written by a new version and never using a new feature still opens in the old version. A change to the major number requires `rupg-cli copy`, and a release that changes it ships the converter.

## 8.16 Crash tests and gates

The crash tests are part of the M1 gate. Document 21 specifies the harness and the deterministic simulation that runs them. This section specifies what they must prove about the file.

| Test | Method | Pass condition |
|---|---|---|
| process kill | kill the owner with `SIGKILL` at random times during a mixed workload | every committed transaction is present, no uncommitted one is, `rupg verify` is clean |
| power loss | a block device that drops every write not followed by a sync, `dm-log-writes` on Linux, a simulated device in document 21 | as above, at every sync point of a recorded run |
| torn writes | the simulated device writes a random prefix of each 4 KiB unit at the crash point | as above |
| lost write | the simulated device acknowledges a write and drops it | the read fails with `XX001`, never returns old data |
| misdirected write | the simulated device writes to another location | the read fails with `XX001` |
| bit flips | flip random bits in pages, extent blocks, slots and the identity block | every flip in a used byte is detected, the file opens if one slot is good |
| slot loss | destroy one slot at random | the file opens from the other slot with no committed data lost after redo |
| full disk | the simulated device returns `ENOSPC` | the statement fails with `53100`, the file stays consistent and opens |
| wrap | run each ring through 100 wraps with crashes | recovery finds the correct end of each ring |

The method of recording a write trace and replaying every crash point that it allows comes from ALICE (Pillai and others, OSDI 2014) and CrashMonkey (Mohan and others, OSDI 2018). Both found crash bugs in mature storage systems that random kill tests did not find.

**The M1 gate is 10,000 crash points with zero failures.** The 10,000 points are spread across the tests above and across the M1 workloads: inserts, updates and deletes through the Rust KV-level API, checkpoints, ring growth and file growth. The number is a budget. It must cover every step of section 8.6.2 and section 8.6.3 at least 100 times, and the harness reports the coverage of each step. At M4 the same suite runs with segment publication, the mover and bulk load, and at M5 with the full transaction layer. Every later milestone keeps it green.
