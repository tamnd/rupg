# 17. Embedding

Written 7 October 2026.

This document specifies how a program uses rupg in its own process. It covers the Rust crate, the C library with the libpq exports, the WebAssembly module, the CLI, the ownership of a file by one process, and the way many processes share one file. The rule from document 00 is that the library is the engine: the server is a thin crate over the library, and anything the server can do, the library can do in process. The crates are `rupg` (the Rust API), `rupg-capi`, `rupg-wasm` and `rupg-cli`, all at rank 13. Most of this work is milestone M9. The Rust API and file ownership come earlier, because the test harness of M0 to M8 runs in process.

## 17.1 The ways in

| Interface | Crate | Who uses it | Transport | Milestone |
|---|---|---|---|---|
| Rust API | `rupg` | Rust programs, the server, the tests | function calls | M1, complete at M9 |
| C API and libpq exports | `rupg-capi` | C and C++ programs, and every language that wraps libpq | function calls | M9 |
| WebAssembly module | `rupg-wasm` | browsers, and JavaScript runtimes | function calls inside the module | M9 |
| CLI | `rupg-cli` | people and scripts | function calls | M3, complete at M9 |
| Embedded listener | `rupg` | drivers that speak the wire protocol themselves | Unix socket or TCP inside the process | M9 |
| Owner channel | `rupg` | a second process that opens the same file | local socket | M2 |

Every way in uses the same parser, catalog, executor and file format. A `.rupg` file written by the browser opens in the native library and in the server with no change, and the reverse is also true.

## 17.2 The library is the engine

**One code path.** The server opens a `Database` through the Rust API and adds the listeners, authentication, TLS and the process model. A session in the library is the same session object as a session in the server (document 06). It has the same settings, the same error fields and the same transaction states. The only difference is the sink that receives results: the wire encoder in the server, and a result buffer in the library (document 04).

**Threads.** The library has no threads unless the program asks for them. By default the thread that calls a statement runs it. `rupg.worker_threads` sets a pool for parallel query, default 0 in the library and the number of cores in the server. A background task such as the page writer, the mover or a checkpoint runs on the pool when it exists, and otherwise in slices on calling threads at statement boundaries. A slice is bounded by the morsel size of about 1 ms. When the program calls `Database::maintain()` on a thread of its own, the background work runs there.

**Memory.** `rupg.memory_limit` defaults to 256 MiB in the library (document 04). The buffer pool, the undo, the operator memory and the caches share it.

**Settings live in the file.** The library has no `postgresql.conf`. `ALTER SYSTEM` writes to a settings table in the file, and the options that the program passes to `open` override it for that process. The server reads its configuration file first and then the settings table, so a file carries its settings from the library to the server. No configuration file sits beside the database.

**Authentication.** A process that can open the file for writing can change every byte of it, so authentication in the library cannot protect anything. A library connection logs in as the role named in its options, default the bootstrap superuser of the file, with no password. Roles, `SET ROLE`, privileges and row level security still apply to the session, because applications use them for logic and not only for security. `pg_hba.conf` rules apply only to the server.

**Fork.** A `Database` must not be used in a child process after `fork()`. The handle records the process id at open. A call from another process id fails with `55000` "database handle used after fork", and the child must open the file again. SQLite has the same rule.

## 17.3 The Rust API

The API is close to the `postgres` and `tokio-postgres` crates, so that code can move between them.

```rust
let db = rupg::Database::open("app.rupg", rupg::Options::default())?;
let mut conn = db.connect()?;
conn.execute("CREATE TABLE t (id int PRIMARY KEY, name text)", &[])?;
conn.execute("INSERT INTO t VALUES ($1, $2)", &[&1i32, &"a"])?;
let mut tx = conn.transaction()?;
for row in tx.query("SELECT id, name FROM t WHERE id > $1", &[&0i32])? {
    let id: i32 = row.get(0)?;
    let name: &str = row.get(1)?;
}
tx.commit()?;
```

**Types.** `Database` is `Send` and `Sync`, and it is cheap to clone. `Connection` is `Send` and not `Sync`, because a session runs one statement at a time. `Transaction` rolls back when it is dropped without `commit()`. `Statement` holds a prepared plan, and its plan stays in the session's plan cache as in the server.

**Values.** Parameters and columns use the traits `ToSql` and `FromSql`, defined over the PostgreSQL type OIDs. A feature `postgres-types` lets types that implement the traits of the `postgres-types` crate pass through unchanged. `row.get::<&str>` borrows from the result buffer and copies nothing.

**Async.** `Database::connect_async()` returns a connection whose methods are futures. They depend on no runtime. A statement that waits for a lock, a commit or I/O returns `Pending`, and the engine wakes the future. The blocking API is the async API driven to completion on the calling thread.

**Bulk paths.** `copy_in` and `copy_out` take and give the `COPY` text, CSV and binary formats. `query_arrow` returns Arrow record batches, behind the feature `arrow`, and it fills them from column vectors without a row step. Embedded analytics in Python and Rust uses Arrow, and DuckDB's Arrow path is the reference that users compare with.

**Errors.** `rupg::Error` carries every field of an `ErrorResponse`: severity, SQLSTATE, message, detail, hint, position, schema, table, column, constraint and the source location. `error.code()` returns the SQLSTATE. Notices go to a callback that the program sets on the connection.

**LISTEN.** `conn.notifications()` returns the queued notifications, and the async form returns a stream.

**In memory.** `Database::open_in_memory()` makes a database whose file is a buffer in memory, with the same format. It has no durability and no rings. `Database::save_to(path)` writes it to a file, with a clean close.

**At M1.** M1 has no SQL, so the M1 API is a key-value API over typed tables. `Database::open` opens or makes a file and replays the log when the file was not closed cleanly. `create_table` makes a table with typed columns, and a `Transaction` does `insert`, `update`, `delete`, `get` and a scan of a range of row ids. Each change is one command, so a later read of the same transaction sees it. `create_table` writes a checkpoint, so a table is in the catalog before any commit to it is in the log. A transaction that starts when the log holds more than `Options::checkpoint_log` bytes after the redo position writes a checkpoint first. `close` writes a checkpoint that marks a clean close. The catalog is one page of kind 4 that the header slot names. It holds the next transaction id, the next OID, and for each table its columns, its hot store root and its row id counter. The catalog tree of `rupg-catalog` takes its place at M2. M1 takes no owner lock, so two processes must not open the same file. The code is in `crates/rupg/src/database.rs` and `crates/rupg/src/catalog.rs`.

## 17.4 One owner for each file

### 17.4.1 The rule

The first process that opens a file becomes its owner. The owner runs the engine for that file: the buffer pool, the log rings, the transactions and the background tasks. A second process that opens the same file connects to the owner through a local socket and runs its sessions in the owner. It sees the same transactions, the same locks and the same `LISTEN` channels. No memory is shared between processes, and no file except the `.rupg` file is created.

This differs from SQLite, where every process runs its own engine and coordinates through file locks and a shared memory file, and from DuckDB, where a second process can only open a file read-only. A single owner keeps the buffer pool, the per-worker rings and the lock manager in one place, which is what the design of document 11 needs. Its cost is that a statement from a second process pays a socket round trip. On gpc, `SELECT 1` to PostgreSQL over a Unix socket takes about 13 µs (document 02), and a statement from a second process costs about the same.

### 17.4.2 Taking ownership

Document 08 owns the lock and the owner record. The owner holds an exclusive lock that the operating system releases when the process ends, so a crashed owner leaves no stale lock.

| Platform | Lock |
|---|---|
| Linux | `fcntl` with `F_OFD_SETLK` on the owner byte range |
| macOS | `flock` with `LOCK_EX \| LOCK_NB` on the whole file |
| Windows | `LockFileEx` with `LOCKFILE_EXCLUSIVE_LOCK` on the owner byte range |
| WebAssembly | the exclusive `FileSystemSyncAccessHandle`, and a Web Lock that names the owner tab (section 17.9) |

The owner then writes the owner record of document 08: its process id, protocol version, start time, the file id, the host identity and the socket address. The record holds no secret.

### 17.4.3 The owner channel

The socket is not beside the file, because the directory of the file can be read-only or shared.

| Platform | Socket |
|---|---|
| Linux | a Unix socket in `$XDG_RUNTIME_DIR/rupg/`, or `/tmp/rupg-` and the user id when that variable is not set |
| macOS | a Unix socket in `$TMPDIR/rupg/` |
| Windows | a named pipe `\\.\pipe\rupg-` and the file id |

The directory has mode 0700, and the socket name is the file id in hexadecimal. When the path is longer than the limit of document 08, the owner uses `/tmp/rupg-` and the user id. A second process reads the owner record, checks the checksum, the file id and the host identity, connects to the address, and sends a startup message for protocol 3.2.

**Who can connect.** A process that can open the file can read every byte of it, so the right to open the file is the right to connect. The second process proves it. On Linux and macOS, it sends its open descriptor of the `.rupg` file with `SCM_RIGHTS` before the startup message. The owner calls `fstat` on the descriptor and accepts the connection only if the device and inode are those of its own file. A descriptor opened read-only gives a session that rejects writes with `25006`. The owner also reads the peer credentials (`SO_PEERCRED` on Linux, `getpeereid` on macOS) and shows the peer's user in `pg_stat_activity`. On Windows, the pipe's security descriptor admits only the owner's user SID, and the owner checks the client's SID with `GetNamedPipeClientProcessId` and the process token.

The protocol on the channel is the PostgreSQL wire protocol, so the owner serves it with the server's code and the second process uses the client of `rupg-wire`. In the second process, `Connection` is the same type with a remote session inside. The program cannot tell, except by the round trip.

**When the connection fails.** A torn or stale record, or a socket that does not answer, makes the second process retry after a short delay, up to `rupg.owner_connect_timeout`, default 10 s, a budget. Two containers that share a file on a volume but not the runtime directory cannot connect, and a file on another host has a different host identity. In both cases the open fails with `55006` "database file is in use by another process" and the owner's process id in the detail.

### 17.4.4 When the owner goes away

**A clean close.** When the owner closes the database and other processes are connected, it stops accepting new sessions, waits up to `rupg.handover_timeout` (default 5 s, a budget) for their transactions to end, ends their sessions with `57P01`, writes a clean checkpoint and releases the lock. The library in each other process then opens the file again. One of them takes the lock and becomes the owner. The others connect to it. The file is clean, so no recovery runs.

**A crash.** The lock is released by the operating system. The other processes see the socket close, and their running statements fail with `08006`. A session that was idle reconnects without an error at its next statement. A session that was in a transaction gets `08006`, because its transaction is lost. The new owner runs recovery (document 11 section 11.16).

**Read-only.** With `rupg.read_only` on, the sessions of the process reject writes with `25006`. If an owner exists, the process connects to it as above. If none exists, the process takes no lock and writes nothing to the file. When the file is not clean, it redoes the log into memory only, as document 08 specifies. This is the mode for read-only media and for files inside an application bundle.

**Network file systems.** Locks and `O_DIRECT` are not reliable on NFS, SMB and FUSE file systems. rupg refuses to open a file on them unless `rupg.allow_network_fs` is on, as document 08 specifies.

## 17.5 Opening, closing and size

**Open.** Opening a file reads the header slots, takes ownership, reads the newest checkpoint, and runs recovery when the file is not clean. The built-in catalog of PostgreSQL 19 is static tables compiled into the binary (document 07), so an open reads only the user catalog. The budget for opening a clean file with 100 tables is 5 ms on gpc. It is a budget.

**Close.** A clean close writes a checkpoint after which every ring is empty (document 11 section 11.15). In the library, a ring is allocated at the first commit and every ring is released at a clean close, and the file is truncated after its last used extent. The file can then be copied, mailed or committed to a repository as one file. The ring extents of document 08 are large, so document 24 asks for a smaller extent in the library.

**Empty size.** A new database with no user tables, after a clean close, must be at most 256 KiB. It is a budget. It holds the header slots, the checkpoint, the settings table and the empty user catalog.

**Create.** `Database::open` creates the file when it does not exist and `Options::create` allows it. The new file has one database named by the option `dbname`, default `postgres`, the roles of a fresh `initdb` and the encoding UTF8. `CREATE DATABASE` adds databases to the same file, as in a PostgreSQL cluster.

## 17.6 The C API and the libpq exports

### 17.6.1 The native C API

`rupg.h` declares a small set of functions for what libpq has no name for.

```c
int rupg_open(const char *path, const char *options, rupg_db **db);
int rupg_close(rupg_db *db);
PGconn *rupg_connect(rupg_db *db, const char *conninfo);
int rupg_query_arrow(PGconn *conn, const char *sql, int nparams,
                     const char *const *values, struct ArrowArrayStream *out);
int rupg_maintain(rupg_db *db, int timeout_ms);
```

`rupg_query_arrow` uses the Arrow C stream interface, so pandas, Polars and DuckDB can read a result with no copy.

### 17.6.2 The libpq exports

The library exports every symbol of libpq's `exports.txt` at the pin, with the same signatures and the same behavior. A program that uses libpq opens a file when its connection string names one:

- `host=/path/app.rupg`, when the path is a regular file whose name ends in `.rupg`. Real libpq reads a host that starts with a slash as a socket directory, so this form needs no new keyword. A program can switch from a server to a file by changing its configuration only.
- `rupg_file=/path/app.rupg`, a new keyword.

Other connection strings connect to a server over the network with the client of `rupg-wire`, with TLS and SCRAM. libpq requests protocol 3.0 by default, and rupg does the same in its libpq, and the server accepts 3.0 and 3.2.

**Asynchronous use.** Many programs drive libpq with `PQsendQuery`, `PQconsumeInput` and `PQgetResult` and wait on `PQsocket()` with `poll` or an event loop. An embedded connection has no socket. `PQsocket()` returns a file descriptor that becomes readable when a result is ready: an `eventfd` on Linux, a pipe on macOS and a socket pair on Windows. Pipeline mode works with the same descriptor.

**Cancel.** `PQcancel`, `PQrequestCancel` and the cancel functions of PostgreSQL 17 set the cancel flag of the session (document 11 section 11.8.4).

**Notifications and notices.** `PQnotifies`, `PQsetNoticeReceiver` and `PQsetNoticeProcessor` work as in libpq.

**What does not apply.** `PQsslInUse` returns 0 for a file. `PQbackendPID` returns the session's backend id. `PQconnectionUsedPassword` returns 0.

### 17.6.3 Two builds

**`librupg`** has its own soname and exports `rupg_*` and the `PQ*` symbols. A program links it in place of libpq.

**The drop-in** is built with the soname `libpq.so.5` on Linux, `libpq.5.dylib` on macOS and `libpq.dll` on Windows. A program that is already linked to libpq uses it through the library path, with no relink. psql linked to the drop-in opens a `.rupg` file, which gives the real psql against an embedded file.

The drop-in must also be a full network libpq, because a program that loads it loses the real one. The network features of libpq are large: TLS options, GSSAPI, service files, the password file, multiple hosts, `target_session_attrs`, load balancing and the OAuth flow of PostgreSQL 18. M9 ships the drop-in with TLS, SCRAM, service files, the password file, multiple hosts and `target_session_attrs`. GSSAPI and OAuth are open in document 24.

## 17.7 Other languages

A language that wraps libpq gets the file through the drop-in or through `librupg`: psycopg in its C form, Ruby's `pg`, PHP's `pgsql` and `pdo_pgsql`, Perl's `DBD::Pg`, R's `RPostgres` and libpqxx.

A driver that speaks the wire protocol itself needs a socket: pgJDBC, Npgsql, asyncpg, node-postgres, Postgres.js and Go's pgx. For these the program sets `rupg.listen`, and the library opens a listener inside the process, on a Unix socket path or on `127.0.0.1` with port 0 for a free port. The listener uses the server's code. pgJDBC 42.7.6 and later can use protocol 3.2 with `protocolVersion=3.2`.

Bindings that skip the protocol come later and are not in M9: a Python module over the Rust API with Arrow results, and a Node.js addon. Document 24 keeps their order open.

## 17.8 Windows

Windows is a tier 2 platform from M9, for the server and the library.

| Concern | Windows form |
|---|---|
| I/O | I/O completion ports, one for each worker |
| Direct I/O | `FILE_FLAG_NO_BUFFERING` with aligned buffers |
| Durable write | `FILE_FLAG_WRITE_THROUGH` on the log writes, and `FlushFileBuffers` for a checkpoint |
| Ownership | `LockFileEx` on the owner range |
| Owner channel | a named pipe |
| Virtual memory | `VirtualAlloc` with `MEM_RESERVE` for the buffer pool, and `MEM_DECOMMIT` for eviction |
| Paths | UTF-16 with long path support |
| Server | a Windows service, and a console mode |

`MEM_DECOMMIT` takes the place of `madvise(MADV_DONTNEED)` in the vmcache design. Document 08 section 8.8 owns the buffer manager, and the Windows form is one more implementation in `rupg-platform`.

## 17.9 WebAssembly

### 17.9.1 The build

The browser build targets `wasm32-unknown-unknown` with JavaScript glue. It runs in a dedicated Web Worker. It has one thread, which is the scheduler's only worker. The kernels use the portable path with 128-bit SIMD (document 04). The compile tiers of `rupg-jit` are not in the build at M9, so queries run on the vectorized interpreter. A later tier can emit WebAssembly and instantiate it, and document 24 keeps that open.

A build for WASI runtimes is not in M9.

### 17.9.2 Storage

The file is a file in the Origin Private File System, opened with `FileSystemSyncAccessHandle`, which has synchronous `read`, `write`, `flush`, `getSize` and `truncate`. These calls exist only in dedicated workers, which is why the module runs in one. The format is the same byte for byte. `db.export()` returns the file as a `Blob`, and the native library opens it.

The buffer manager cannot reserve virtual memory, so it uses the hash table from page number to frame (document 08 section 8.8). The address space of `wasm32` is 4 GiB, and the default `rupg.memory_limit` of 256 MiB is far below it. A `memory64` build is open in document 24.

**Durability.** A commit calls `flush()` on the handle. What `flush()` promises about the disk is the browser's choice, and rupg cannot make it stronger. The documentation says so. A crash of the tab is the same as a crash of a process: the lock goes away and the next open runs recovery.

### 17.9.3 Many tabs

The sync access handle is exclusive, so one tab owns the file, as one process does natively. Ownership uses the Web Locks API: the worker that holds the lock named `rupg:` with the file path opens the handle. Other tabs send wire protocol messages to the owner over a `BroadcastChannel` and receive the replies there. When the owner tab closes, its lock is released and another tab takes it. This is the rule of section 17.4 with browser parts.

### 17.9.4 The JavaScript API

```js
import { Rupg } from "@rupg/wasm";
const db = await Rupg.open("opfs://app.rupg");
await db.exec("CREATE TABLE t (id int PRIMARY KEY, v text)");
const { rows, fields } = await db.query("SELECT * FROM t WHERE id = $1", [1]);
const reply = await db.execProtocol(messageBytes);
```

`query` returns rows as objects with the type parsers of node-postgres by default. `execProtocol` takes and returns raw wire protocol bytes, so a driver that speaks the protocol can run over it. PGlite has a function of the same kind, and ORM adapters for Drizzle, Prisma and Kysely use it. `Rupg.open("memory://")` makes an in-memory database.

### 17.9.5 Size and speed

PGlite is about 3 MB gzipped and allows one connection (document 01). rupg's budget for the module is at most 3 MB gzipped. The large parts are the parser tables, the built-in catalog, the time zone data, the encoding conversion tables and the collation data. The conversion tables for encodings other than UTF8 and the ICU collation data are separate assets that load when a statement first needs them. The built-in collation providers of PostgreSQL 17 and 18 are in the module. This is a budget, and M9 measures it.

F3 (SIGMOD 2025) and AnyBlox (VLDB 2025) measured WebAssembly at 15 percent to 46 percent slower than native code for decoding work. The 10x claims of document 02 are native claims. The WebAssembly build has gates on size, open time and correctness, and its ClickBench and YCSB numbers are published as their own rows without a claim.

## 17.10 Budgets and gates

| Item | Budget or gate | Kind |
|---|---|---|
| prepared point read by primary key through the Rust API, hot, one thread | at most 2 µs | budget |
| statement from a second process through the owner channel | within 2 µs of PostgreSQL's `SELECT 1` round trip on the same machine | budget |
| open of a clean file with 100 tables | at most 5 ms | budget |
| empty database after a clean close | at most 256 KiB | budget |
| idle library with one open database and no statements | at most 8 MiB of resident memory beyond the binary | budget |
| idle session in the library | at most 64 KiB, as in the server (document 06 section 6.4) | gate |
| `librupg` stripped size on Linux x86-64 | at most 25 MiB | budget |
| WebAssembly module | at most 3 MB gzipped | gate at M9 |
| the regression suite through `librupg` with `host=` a file | the same results as through the server | gate at M9 |
| the regression suite in the browser build | the same results, except the tests that need a second process | gate at M9 |
| crash test of the owner while second processes write | every acknowledged commit survives | gate at M9 |

The point read budget of 2 µs has no measured base yet. It is set from the fact that the in-process path removes the socket, the protocol encoding and the context switch, which are most of the 35 µs of a PostgreSQL point read on gpc (document 02). The SQLite paper measured SQLite at 10x to 500x faster than DuckDB on writes and DuckDB at 3x to 50x faster than SQLite on the Star Schema Benchmark (PVLDB 15, 2022). An embedded engine is judged by both sides. Document 20 decides whether the embedded runs of ClickBench and YCSB compare with DuckDB and SQLite as published rows. The 10x claim against DuckDB on ClickBench applies to the library as it does to the server, because DuckDB is itself embedded.
