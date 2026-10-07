# Changelog

All changes that a user can see are in this file. The version is 0.M.patch, where M is the last milestone that closed. See [`spec/23-milestones.md`](spec/23-milestones.md) section 23.17.

## Unreleased

## 0.0.7 (2026-10-07)

The seventh patch release on the 0.0 line. It adds the M1 types, the M1 table and the first MVCC with snapshot reads. No crate is published.

### Added

- The M1 types in `rupg-types`. `TypeId` names a type by its OID in `pg_type`, with its name and the width of its storage form. `Datum` holds one value of `bool`, `int2`, `int4`, `int8`, `float4`, `float8`, `oid`, `text`, `bytea`, `uuid`, `date`, `timestamp` or `timestamptz`, and `encode` and `decode` give its storage form. A bad storage form gives `XX001`.
- The M1 table in `rupg-table`. `TableDef` checks the columns of a table and maps each typed column to the storage form of the hot store. `encode` checks a row against the columns with the errors of PostgreSQL for an `INSERT`: `42601` for a wrong value count, `42804` for a wrong type and `23502` for a NULL in a column that is not nullable. `Table` holds the definition, the hot store and the row id counter of one shard.
- The first MVCC in `rupg-txn`. `Transactions` starts a `Transaction` at `READ COMMITTED` or `REPEATABLE READ`, with `get`, `scan`, `insert`, `update`, `delete`, `next_command`, `commit` and `rollback`. A writer changes a row in place and keeps the old version in an undo record, and a reader follows the undo chain to the version that its snapshot sees. A writer waits for the uncommitted owner of a row. `REPEATABLE READ` gives `40001` on a concurrent update or delete, and a wait cycle gives `40P01`. A dropped transaction rolls back. `cleanup` removes undo that no snapshot needs. Spec/11 section 11.4.3 has the M1 note.
- SQLSTATEs `23502`, `42601`, `42701`, `42804`, `42P07` and `54011` in `rupg-common`, and `RowIds::saw` in `rupg-hot`, which recovery uses to move the row id counter past each row that it finds.

### Changed

- More than 1,600 columns give `54011`, as in PostgreSQL, in place of `54000`.

## 0.0.6 (2026-10-07)

The sixth patch release on the 0.0 line. It adds the B+ tree of spec/12, the hot store of spec/10 with its PAX leaves and row id blocks, and the commit clock and the `visible` word of spec/11. No crate is published.

### Added

- The hybrid logical clock and the `visible` word in `rupg-txn`. `HlcClock` gives each commit a timestamp above every value that the node gave or saw, also when the wall clock goes back. `Commits` takes the commit timestamps and keeps `visible` below each commit that is not installed, so a snapshot is one load and misses no commit.
- The B+ tree of `rupg-tree`. `Tree` goes down with optimistic latch coupling over `PageAccess`, latches only the leaf, splits full leaves and inner nodes, and keeps its root page when the root splits. Each user gives its own leaf format through the `Leaf` trait. `scan` reads the leaves in key order, and `check` checks the kinds, the levels and the key bounds. `MemPages` is a `PageAccess` in memory for the tests of this crate and of the crates above it. Spec/12 now gives the inner node format.
- The hot store of `rupg-hot`. `HotStore` keeps the rows of one table and shard in a B+ tree of PAX leaves keyed by row id, with `get`, `insert`, `replace`, `remove` and `scan`. `set_header` and `set_xmax` write in place. A row that does not fit in an empty leaf gives `54000`, as in PostgreSQL. A split at the end of a leaf keeps the leaf full, so rows in row id order fill the leaves. `RowIds` gives row ids in blocks of 1024. `VersionHeader`, `Row` and `Schema` give the row format. Spec/10 now gives the leaf header, the column directory and the minipages, and spec/11 gives the flag bits of the version header.
- SQLSTATE `54000`, `program_limit_exceeded`, in `rupg-common`.

### Changed

- `Tree::scan` in `rupg-tree` copies each leaf and releases the latch before it calls its function, so the function can change the tree.

## 0.0.5 (2026-10-07)

The fifth patch release on the 0.0 line. It adds the log of spec/11: the ring pages and ring extents in the file, the block and record formats, the ring writer with group commit, the safe positions of the rings, and recovery after a crash. No crate is published.

### Added

- The ring directory page, kind 3, and the ring extent list page, kind 18, in `rupg-file`, with `RingEntry`, `RingDirectoryPage`, `RingExtentsPage` and their text forms. Spec/08 now gives their layout and moves the owner of kind 3 to `rupg-file`.
- Log ring extents in `FileStore`. `add_ring_extent` gives out a free arena as an E4 extent, `free_ring_extent` takes it back after two checkpoints, and `write_ring`, `read_ring` and `sync_rings` do the I/O. `Checkpoint` now takes the rings in place of a ring directory root, and the checkpoint writes the ring directory and an extent list for each ring. `FileStore::open` reads and checks them, and `FileStore::rings` gives them.
- The log block and record formats in `rupg-log`. `Block` encodes and decodes a block with its 40 byte header, its dependencies and a CRC32C seeded with the ring number. A bad checksum or position is the end of a ring, and other damage gives `XX001`. `RecordWriter` and `RecordReader` write and read the records of a block body with delta encoded OIDs and row ids. Spec/11 now gives the offsets, the kind numbers and the fill block.
- The log ring writer in `rupg-log`. `Ring` places blocks in a ring, pads the end of a 4 KiB unit and of an extent with a fill block, and flushes with group commit. `RingReader` reads the blocks of a ring until its end. `Log` keeps the safe position of each ring and waits for the dependencies of a commit.
- Log recovery in `rupg-log`. `Recovery::scan` reads each ring from its redo position, cuts the rings on their dependencies and gives the safe blocks, without the part blocks that no commit names. `Recovery::finish` writes a checkpoint that starts each ring one ring size after its end, so a block that a stopped flush left after a hole is never read again. `Ring::open` now takes only the ring state of a checkpoint. Spec/11 now says that a redo position must not pass the first part block of a running transaction, and gives the new recovery step.

### Changed

- The simulated disk grows a file with a copy from a zeroed buffer, which is much faster than `Vec::resize` in a debug build.

## 0.0.4 (2026-10-07)

The fourth patch release on the 0.0 line. It adds the file store of spec/08 with out-of-place page writes, the page table in memory and the checkpoint, so the buffer pool can keep its pages in a database file. No crate is published.

### Added

- Page kind 17 for the pending free list in `rupg-file`, with `FreeListPage` and its text form. Spec/08 now says that a superseded page waits for two checkpoints, because the old slot still uses it after the first one.
- `FileStore` in `rupg-buffer`, the store of a database file. It writes each page out of place in a write arena and keeps the page table in memory. `FileStore::checkpoint` writes the changed page table nodes, the pending free list and the free space map, syncs, writes the inactive header slot and syncs again. `FileStore::open` chooses the slot and checks each metadata page against its slot or its parent entry. An error during a sync or a checkpoint stops the store.

## 0.0.3 (2026-10-07)

The third patch release on the 0.0 line. It completes the file format of spec/08 with the page table, the extents and the free space map, and adds the buffer manager with the virtual memory window and the memory budget. No crate is published.

### Added

- The page table of spec/08 section 8.5 in `rupg-file`: `PtEntry` with the 48-bit physical page and the 16-bit write tag, `PtPath` for the radix walk, and `PtNode` for the 2040 entries of a node. A table has 3 levels and grows to 6 when the logical page numbers need it.
- The extent classes E0 to E4 with `Extent`, which checks the alignment inside the arena, and the checksum of each 64 KiB block.
- The arenas and the free space map of spec/08 section 8.9: `ArenaMap` with the buddy rule and the in-order pages of a write arena, the 8-byte `FsmEntry`, and `FsmPage` with 120 arenas in each page. A double free and an entry that does not match its bitmap are errors.
- The text form for page table nodes and free space map pages. It parses back into the same sealed page.
- The `PageAccess` trait in `rupg-file`, with the optimistic, shared and exclusive modes of spec/08 section 8.8.2 and the `OptimisticRead` trait for reads without a latch.
- `BufferPool` in `rupg-buffer`. It has the vmcache state word for each frame, a clock that evicts frames and writes dirty pages first, and a fixed table of frames that it reserves from the memory budget (spec/08 section 8.8.4). It implements `PageAccess` over a `Store`. `MemStore` keeps the pages in memory.
- The virtual memory window of spec/08 section 8.8.1 in `BufferPool` on Linux and macOS. The bytes and the state word of logical page `n` are at index `n`, so a hit needs no lookup. The pool grows from `shared_buffers` in steps of 64 pages, and its reclaimer gives clean pages back when another consumer of the memory budget needs them. `PoolConfig` selects the window or the table, and other hosts use the table.
- `Region` in `rupg-platform`, a reservation of address space with `mmap` and `MAP_NORESERVE`. `Region::discard` gives the memory of a range back to the operating system.

## 0.0.2 (2026-10-07)

The second patch release on the 0.0 line. It has the error type and the identifiers of `rupg-common`, the platform traits with their simulation, and the first part of the file format. No crate is published.

### Added

- `Error` and `SqlState` in `rupg-common`. Each error has the SQLSTATE that PostgreSQL sends for the same cause. A test checks each constant against the `errcodes.txt` of the pin.
- The identifiers of spec/04 section 4.7 in `rupg-common`: `Oid`, `ShardId`, `RowId`, `Hlc`, `Xid` and `Lsn`. Each has a text form that parses back. `Hlc::tick` and `Hlc::merge` keep the clock monotonic when the wall clock goes back. `Xid::next` skips the 32-bit values 0, 1 and 2.
- The `Io`, `File`, `Clock` and `Entropy` traits in `rupg-platform`, with the operating system implementations `OsIo`, `OsClock` and `OsEntropy`. On macOS the sync is `F_FULLFSYNC`.
- The simulation implementations `SimIo`, `SimClock`, `SimEntropy` and the seeded generator `SimRng`. `SimIo` keeps the files in memory with the power cut model of spec/21 section 21.9.1: unsynced writes can be lost, written, torn at a 4 KiB or 512 byte boundary, misdirected or reordered. It also injects `EIO` and `ENOSPC`.
- The `Tasks` and `Net` traits with `OsTasks` and `OsNet` (threads and TCP), and `SimTasks` and `SimNet`. `SimTasks` runs the tasks on the calling thread in an order that the seed picks. `SimNet` connects listeners and clients in memory.
- `MemoryPool`, the one memory budget of spec/04 section 4.6. A reservation that does not fit asks a reclaimer for memory, then fails with `53200`. A test mode fails the Nth reservation.
- `CpuFeatures` and `CpuLevel`, which pick the kernel path at run time, and `physical_memory`.
- The four-lane page checksum in `rupg-file`, lifted from rudb. It gives the same result as XXH64 with seed 0, and the tests check the reference vectors.
- The fixed blocks of the `.rupg` file in `rupg-file`: the identity block, the header slots A and B, and the owner record. `choose_slot` takes the valid slot with the higher generation by the rule of spec/08 section 8.2.2 and says why. A file with no valid slot gives `XX001` and names `rupg inspect --salvage`.
- The 64-byte page header with the 16 page kinds. `verify` finds a bad checksum, a misdirected write and a lost write, each with `XX001`.
- The text form of spec/08 section 8.14 for each of these blocks. It has one field on each line, sorted, and it parses back.
- The SQLSTATE `22P02` for a text form that does not parse.
- `clippy.toml` now rejects `std::fs`, `std::net`, `std::thread` and `RandomState::new` outside `rupg-platform`, as spec/22 section 22.1 requires. `xtask` allows them.

### Fixed

- The release workflow ran the Windows build in PowerShell, where `$TARGET` is not the environment variable, so the build had no target and failed. The step now runs in bash. Release 0.0.1 has no binaries for this reason.
- `clippy.toml` named two Unix socket types that do not exist on Windows, and clippy warned on a Windows build. The two entries now allow this.

## 0.0.1 (2026-10-07)

The first patch release on the M0 line. M0 is not closed, so this release has no measured number. It has the workspace, the vendored files of the pin, the LALR(1) check of the grammar and the CI. No crate is published.

### Added

- The workspace with the 41 crates of `spec/22-crate-layout.md`. Most crates are stubs until their milestone.
- `cargo xtask layers`, which checks the layer rule from `xtask/layers.toml`.
- `cargo xtask style`, which checks `forbid(unsafe_code)`, `SAFETY` comments and the writing rules.
- The `rupg` binary with `--version` and `--print-config`.
- `CompatVersion` for PostgreSQL 14 to 19 in `rupg-common`.
- The technical specification in `spec/`.
- `vendor/` with the PostgreSQL files of the pin and of 14.24, 15.19, 16.15, 17.11 and 18.6, the time zone data 2026e and the Unicode character database 17.0.0. Each directory has a `PIN` file with the source, the revision, the license and the SHA-256 of each file.
- `cargo xtask vendor`, which fetches the vendored files, and `cargo xtask vendor --check`, which checks them with no network.
- `cargo xtask grammar`, an LALR(1) generator that reads a bison grammar and checks the conflict counts against `%expect`. It gives the same tables as bison 3.8.2 for `gram.y` (6574 states) and `pl_gram.y` (336 states).
- `cargo xtask version`, which sets the workspace version, the exact internal pins and `Cargo.lock` together.
- CI with the matrix of `spec/21-testing.md` section 21.16, nightly sanitizers and Miri, a release workflow with build provenance, and security scans.
