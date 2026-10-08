# Changelog

All changes that a user can see are in this file. The version is 0.M.patch, where M is the last milestone that closed. See [`spec/23-milestones.md`](spec/23-milestones.md) section 23.17.

## Unreleased

### Added

- Aggregate functions, `GROUP BY` and `HAVING` (spec/23 section 23.5). The analyzer makes an aggregate call as `parse_agg.c` does, with `DISTINCT`, `ORDER BY` and `FILTER` in the call, and gives the same errors as PostgreSQL for an aggregate call in a wrong place, a nested call, and a column that is not in `GROUP BY`. A column of a table whose primary key is in `GROUP BY` is grouped too. The executor runs the transition and final functions of `pg_aggregate` for `count`, `sum`, `avg`, `min`, `max`, `bool_and`, `bool_or`, `every`, `bit_and`, `bit_or`, `bit_xor`, `any_value`, `string_agg`, `array_agg` and the `float4` and `float8` forms of `var_pop`, `var_samp`, `variance`, `stddev_pop`, `stddev_samp` and `stddev`. The sums and the averages of `bigint` and `numeric` keep their state as `numeric.c` does, with the count of `NaN` and of the infinite values. The groups come out in the order of their keys, and the rows of a group keep the order of the scan. A `GROUP BY` on a type that has `=` and no order, such as `xid`, keeps the groups in the order of their first rows. A `GROUP BY` that can neither sort nor use a hash table gives `could not implement GROUP BY`, as PostgreSQL does. The ordered-set aggregates, the window functions, `GROUPING SETS`, `ROLLUP`, `CUBE`, the JSON aggregates, `array_agg` of an array, and the `var_pop` and `stddev` forms of the integer and `numeric` types give `0A000`.

### Fixed

- For two equal values, the `larger` and `smaller` functions such as `numeric_larger` and `float8smaller` give the second value, as in PostgreSQL. The functions of `bpchar` and `tid` give the first value, as in PostgreSQL.

## 0.0.16 (2026-10-08)

The sixteenth patch release on the 0.0 line. `SELECT` now reads the tables of the system catalog with `FROM` and joins, and sorts and cuts the result with `ORDER BY`, `DISTINCT`, `LIMIT` and `OFFSET`. No crate is published.

### Added

- `FROM` over the tables of the system catalog (spec/23 section 23.5). A query reads the tables of `pg_catalog`, by name or with the schema, with aliases and column aliases, and joins them with `JOIN ... ON`, `USING`, `NATURAL`, `CROSS JOIN` and a comma. `INNER`, `LEFT`, `RIGHT` and `FULL` joins work, and a `USING` alias names the merged columns. The analyzer finds the columns as `parse_relation.c` does, so a bad name gives the same error, detail and hint as PostgreSQL, with the close names that `Perhaps you meant to reference the column` suggests. `*` and `t.*` expand in the target list, and `tableoid` is the OID of the table. `RowDescription` gives the table OID and the attribute number of each plain column. The executor reads only the columns that the query uses, tests each part of `WHERE` on the smallest part of `FROM` that has its relations, and joins on an `=` of two columns through an index of one side. The other system columns, a whole-row reference, and a subquery or a function in `FROM` give `0A000`. The rows that `initdb` adds after the bootstrap, such as the ACLs and `plpgsql`, are not there yet.
- `ORDER BY`, `DISTINCT`, `DISTINCT ON`, `LIMIT`, `OFFSET` and `FETCH FIRST ... WITH TIES` (spec/23 section 23.5). The analyzer finds each item of `ORDER BY` as `parse_clause.c` does: an output column name, a position in the select list, or an expression that adds a junk column. `ASC`, `DESC`, `USING`, `NULLS FIRST` and `NULLS LAST` work, and the operators of a type come from its default btree or hash operator class, as `typcache.c` finds them. A bad item gives the same error and position as PostgreSQL. The executor sorts with a port of `pg_qsort` and of the bounded heap of `tuplesort.c`, so the rows that are equal on all keys come in the same order as PostgreSQL gives for the same input order. A `Sort` below `LIMIT` keeps only the first `OFFSET` + `LIMIT` rows, as PostgreSQL does. A `DISTINCT` on a type that has `=` and no order keeps the first row of each group. The rows of the catalog do not come in the order of the heap of an oracle after `initdb` yet, so equal rows can come in a different order. PostgreSQL can also use an index scan or a hash table where rupg sorts, and then the equal rows and the rows of a `DISTINCT` without `ORDER BY` can come in a different order. rupg does not fold the constant expressions before it runs a query, so `ORDER BY 1/0 LIMIT 0` gives no rows and no error.

### Fixed

- An `int2vector` and an `oidvector` have the lower bound 0, as in PostgreSQL, so a cast to an array shows `[0:1]={1,2}`.
- The comparison operators of `oidvector` compare the number of elements first, as `btoidvectorcmp` does. They compared as arrays before.
- A cast between two array types whose element types have the same binary form, such as `int2vector` to `int2[]` and `oidvector` to `oid[]`, works now. It gave `0A000` before.

## 0.0.15 (2026-10-08)

The fifteenth patch release on the 0.0 line. It adds the first part of the query engine: `SELECT` without `FROM`, through the simple and the extended query protocol, and the status of each parameter. No crate is published.

### Added

- The status of each parameter (spec/06 section 6.13.2). rupg honors 37 parameters, accepts 384, and refuses some values of 4. A refused value fails with `0A000` and `rupg does not support parameter "%s" set to "%s"`, from `SET`, `BEGIN`, the startup packet, the command line or the configuration file. The refused values are `serializable` for `transaction_isolation` and `default_transaction_isolation`, `replica` for `session_replication_role`, and `on` for `ssl_sni`. `crates/rupg-session/tests/status.tsv` is the report.
- `SELECT` without `FROM` (spec/23 section 23.5). The analyzer resolves the names, the operators, the functions and the casts as `parse_func.c`, `parse_oper.c` and `parse_coerce.c` do, with the unknown literals, the polymorphic types and the implicit casts. The executor evaluates the expressions with `WHERE`. The functions of `rupg-func` cover the operators and the casts of the scalar types of `rupg-types`, the string functions, `version()`, `now()`, `current_setting`, `format_type`, `pg_typeof`, `current_schemas` and other functions that read the session. A function that has no kernel yet gives `0A000`. The simple and the extended query protocol run the statement, and `Parse` gives the types of the parameters and of the columns. A bad parameter value in `Bind` gives the `CONTEXT` field of PostgreSQL, which `log_parameter_max_length_on_error` cuts. `now()` is the start of the transaction and `statement_timestamp()` is the start of the statement, as in PostgreSQL. `TimeZone` takes UTC, GMT, `Etc/GMT+N` and the fixed offsets. Other named zones give `0A000` when a statement needs the zone. The parameters `DateStyle`, `IntervalStyle`, `extra_float_digits`, `bytea_output`, `array_nulls`, `search_path`, `TimeZone` and `log_parameter_max_length_on_error` are honored now.

### Fixed

- The parameter table follows an oracle build with lz4 and zstd, as spec/05 and spec/09 say. `default_toast_compression` is `lz4` by default and also takes `lz4`, and `wal_compression` also takes `lz4` and `zstd`. `data_checksums` shows `on`, as spec/08 section 8.3 says.

## 0.0.14 (2026-10-08)

The fourteenth patch release on the 0.0 line. It adds TLS to the server, the extended query protocol and cancel. `psql` 19 can connect with `sslmode=verify-full` and channel binding, run prepared statements and pipelines, and cancel a statement. No crate is published.

### Added

- TLS on the server with rustls and its `ring` provider (spec/06 sections 6.5 and 6.6). With `ssl = on`, the server loads `ssl_cert_file` and `ssl_key_file` at start and checks the owner and the mode of the key as PostgreSQL does. A TCP connection starts TLS with an `SSLRequest`, or with direct TLS and the ALPN protocol `postgresql`. The Unix socket has no TLS. The rules `hostssl` and `hostnossl` match. A TLS connection offers `SCRAM-SHA-256-PLUS` with the channel binding `tls-server-end-point`. With `ssl_ca_file`, the server asks for a client certificate, and the method `cert` and the option `clientcert` check it. `ssl_crl_file` gives the revoked certificates. `ssl_min_protocol_version` and `ssl_max_protocol_version` set the versions, and rustls has TLS 1.2 and 1.3 only. `ssl_ciphers`, `ssl_tls13_ciphers`, `ssl_groups`, `ssl_dh_params_file` and `ssl_crl_dir` are accepted and not used yet, and `ssl_sni = on` stops the start. `rupg serve` reads a relative path of a TLS file from the current directory. The feature `tls` of `rupg-server` and `rupg` adds TLS, and the feature `server` turns it on. Without it, `ssl = on` stops the start with the text of PostgreSQL.
- `rupg-platform` gives the owner and the permission bits of a file with `Io::mode`.
- The extended query protocol (spec/06 section 6.1): `Parse`, `Bind`, `Describe`, `Execute` with a row limit, `Close`, `Flush` and `Sync`, with the named and the unnamed prepared statements and portals. The errors and their order are the errors of `postgres.c`. The messages up to `Sync` run in one implicit transaction block, so an error rolls back the statements before it, and `Sync` commits. `DISCARD ALL` drops the named prepared statements. A portal ends with its transaction. The result columns of `SHOW` can have the binary format. `Bind` checks each parameter value with the input or the receive function of its type for the types of `rupg-types`. A statement that the session cannot run yet gives `0A000` at `Parse`. The function call protocol still gives `0A000`.
- Cancel (spec/06 section 6.12). A `CancelRequest` with the process ID and the key of a session sets the cancel flag of the session. The session checks the flag where `postgres.c` calls `CHECK_FOR_INTERRUPTS`: before each statement of a `Query`, at the end of `Parse`, and before `Execute` runs a portal. A set flag gives `57014` with `canceling statement due to user request`. A cancel that comes while the session waits for a message does nothing, as in PostgreSQL. A request with a wrong key, with the process ID 0 or with an unknown process ID writes the log line of PostgreSQL.

## 0.0.13 (2026-10-08)

The thirteenth patch release on the 0.0 line. It adds the system catalogs and their static rows, and the first part of the server of milestone M2: `rupg serve` with the simple query flow and the transaction block, the Unix socket with its lock file, and client authentication with `pg_hba.conf`. `psql` can log in with SCRAM, change settings and run transaction blocks. No crate is published.

### Added

- `rupg-pgcatalog` has the schema of the 64 system catalogs and their static rows (spec/07 section 7.13). `cargo xtask pgcatalog` reads the vendored catalog headers and `.dat` files with the rules of `genbki.pl` and of the bootstrap mode: the defaults, the array types, the OID lookups, the OIDs from 10000, the row types of the catalogs, the `aclitem` values, the `Const` nodes of `proargdefaults`, and the descriptions. It also makes the `pg_class`, `pg_attribute` and `pg_index` rows of the catalogs, their 35 toast tables and their 124 indexes, and the 192 `pg_constraint` rows of `system_constraints.sql`. The rows are column batches in the binary. `--check` fails if the files differ, and CI runs it. A test compares the 13,770 rows with the catalogs of the oracle after `initdb`, and each difference that a later step of `initdb` makes is named in the test.
- `vendor/postgres-19` has the 64 catalog headers, `transam.h`, `pg_wchar.h` and `system_functions.sql`.
- `rupg serve [--listen ADDR] [--pwfile FILE] [-c name=value]...` runs the PostgreSQL server. `psql` and other clients connect as the role `postgres` to the database `postgres` or `template1`. The server runs `SET`, `RESET`, `SHOW`, `DISCARD`, `BEGIN`, `START TRANSACTION`, `COMMIT`, `ROLLBACK` and `SET TRANSACTION` with the rules of PostgreSQL, and gives `0A000` for any other statement and for the messages of the extended query protocol. Each connection has its own task at this step.
- `rupg-session` has the main loop of a session after the startup (spec/06 section 6.1): the startup settings, the `ParameterStatus` reports, the simple query protocol of `exec_simple_query`, the transaction block states of `xact.c` without savepoints, and the transaction characteristics that `AND CHAIN` keeps.
- `rupg-server` has the listener, the startup packet, the refusals of an unknown role and database, the process IDs and the cancel keys.
- The facade crate `rupg` has the feature `server`, with `rupg::server::serve`.
- `rupg serve` also listens on the Unix socket `<dir>/.s.PGSQL.<port>` for each directory of `unix_socket_directories`, which is `/tmp` by default (spec/06 section 6.15). The port is the port of the TCP address. Before it listens, the server creates the lock file `<socket>.lock` with the contents of PostgreSQL. If the process in a lock file is alive, the server does not start and gives `F0001` with the hint of PostgreSQL. If the process is gone, the server takes the lock file. `SHOW port` and `SHOW listen_addresses` give the TCP address that the server got.
- Client authentication with the rules of `pg_hba.conf` and the methods `trust`, `reject`, `password`, `md5`, `scram-sha-256` and `peer` (spec/06 sections 6.7 and 6.8). The parser of the rules and of the user maps is lifted from tamnd/rudb at f5f7065a and has the error texts of PostgreSQL, with `include`, `include_if_exists`, `include_dir`, `@file`, host names, `samehost` and `samenet`. The setting `hba_file` names a file of rules, and `ident_file` names a file of user maps. Without them the server uses the defaults of spec/06 section 6.8: with a password, `scram-sha-256` on the Unix socket and on the loopback addresses, and without a password, `trust` on the Unix socket only. Rules that do not load stop the start with `could not load pg_hba.conf`. `--pwfile FILE` reads the password of `postgres` from the first line of the file, and the server stores it as `password_encryption` says. A role that does not exist goes through SCRAM with a mock secret, so the client cannot tell it from a wrong password. The server log on the standard error gets the reason of each failed login, as `errdetail_log` gives it in PostgreSQL. After a login with an MD5 secret, the client gets the warning `01000` of PostgreSQL 19 unless `md5_password_warnings` is off. Regular expressions in the rules, `ident` over TCP and the methods that need TLS are not done yet.
- `rupg-platform` takes the path of a Unix socket as an address of `Net`. A `Stream` tells if it is a Unix socket, and on a Unix socket it gives the user of the operating system on the other side, for the method `peer`.

### Changed

- A thread of `OsTasks` has a stack of 8 MiB, the stack of the main thread on Linux. The parser of a debug build needs more than the 2 MiB that Rust gives a new thread.

## 0.0.12 (2026-10-08)

The twelfth patch release on the 0.0 line. It starts milestone M2: the wire codec, SASLprep, the full SQLSTATE list, the text and binary forms of the types, the configuration parameters, the time zone names and the PostgreSQL parser. Each part is lifted from tamnd/rudb at f5f7065a and checked against PostgreSQL 19 at the pin. No crate is published.

### Added

- `rupg-wire`, the message codec of protocols 3.0 and 3.2 (spec/06 section 6.1). It is a copy of `rudb-pgwire` from tamnd/rudb at f5f7065a, and each file names its source. It has the startup packets, the frontend and backend messages, the SCRAM-SHA-256 messages and keys, the cancel keys, and the table of command tags. A test checks the table against the vendored `cmdtaglist.h`.
- The fuzz targets `wire_startup` and `wire_frontend` (spec/21 section 21.7). They run the small test server of `rupg-wire` on the codec, and the nightly workflow runs them with the storage targets. `wire_startup` starts from the seeds in `fuzz/seeds/wire_startup`.
- SASLprep in `rupg-wire`: `saslprep` and `prepare_password`, as `pg_saslprep` does it. `verify_password` applies it before it checks a SCRAM secret, so a password with characters outside ASCII gives the same result as in PostgreSQL.
- `rupg_types::unicode`, the four Unicode normalization forms from the vendored Unicode 17.0.0 data. It passes every line of `NormalizationTest.txt`.
- `cargo xtask unicode` makes the normalization tables and the SASLprep tables from the vendored files, and `--check` fails if they differ. CI runs the check.
- `rupg_common::SqlState` has every code line of `errcodes.txt`: 268 constants, named as the C macro without `ERRCODE_`, with `parse`, `name`, `condition`, `for_condition`, `class_title` and `category`. `cargo xtask errcodes` makes the list, and CI checks it. The code is lifted from `rudb-common`.
- The text and binary forms of the types in `rupg-types` (spec/07): `bool`, `"char"`, `name`, the integers, `oid`, the floats, `bytea`, `uuid`, `text`, `varchar`, `bpchar`, `json`, `jsonb`, `numeric`, the date and time types, arrays, `int2vector`, `oidvector` and the OID alias types. Each function gives the SQLSTATE and the message of PostgreSQL. The code is lifted from `rudb-pgtypes`, and a test checks 8753 results of the server at the pin.
- `rupg_types::oid`, one constant for each type of `pg_type.dat` and for its array type, with the `pg_type` row of each type. `cargo xtask pgtype` makes the table from the vendored file, and CI checks it.
- `rupg_session::guc`, the configuration parameters of PostgreSQL (spec/06 section 6.13). It reads a value as `SET` does and prints it as `SHOW` does, with the errors of PostgreSQL, and `Settings` keeps the values of one session. `cargo xtask guc` makes the table from the vendored `guc_parameters.dat` and `guc_tables.c` with the boot values of the Linux oracle, and CI checks it. A test checks each of the 419 rows of `pg_settings` of the oracle. The code is lifted from `rudb-common`.
- `rupg_types::tz`, the 598 names of the zones and links of the vendored tzdata 2026e. `cargo xtask tzdata` makes the list, and CI checks it. `TimeZone` takes these names.
- `rupg-sql`, the PostgreSQL parser (spec/07 section 7.2). It is a copy of `rudb-pgparse` from tamnd/rudb at f5f7065a without its transform, and each file names its source. It has the lexer of `scan.l`, the token filter of `parser.c`, the keyword lookup, the LALR(1) tables of the full `gram.y` of the pin, the raw parse tree of the PostgreSQL node headers with the text of `nodeToString`, and an action for each of the 3,476 alternatives. `parse` gives the raw tree, and `check` gives the first error with the SQLSTATE, the message and the position of PostgreSQL. The errors use `rupg_common::SqlState`. A test compares the trees and the errors of 115 statements with those of PostgreSQL 19.
- `cargo xtask sql` makes the tables, the keywords, the node types and the action glue of `rupg-sql` from `gram.y`, `kwlist.h`, the node headers, `errcodes.txt` and `pg_type.dat` in `vendor/postgres-19`, with the state numbers of bison 2.3. `--check` fails if the files differ, and CI runs it. `--bison` runs bison on the same grammar and compares every action and every goto. The generators are lifted from `xtask/src/postgres` of tamnd/rudb.

### Changed

- Three `SqlState` constants have the names of `errcodes.txt`: `SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION`, `T_R_SERIALIZATION_FAILURE` and `T_R_DEADLOCK_DETECTED`. `SqlState::code` is now `SqlState::as_str`, and `SqlState::new` is now `SqlState::parse`.

## 0.0.11 (2026-10-08)

The eleventh patch release on the 0.0 line. It adds the crash tests on real kernels and makes recovery faster. On server1 (4 cores, Linux 6.8), the open of a file with 1 GiB of log after a drop of the page cache takes 9.4 s, and it took 15.3 s before #129. No crate is published.

### Added

- The crash tests on real kernels in `tests/realdisk` (spec/21 section 21.9.3). `logwrites.py` runs the M1 workload on ext4 or XFS over dm-log-writes, replays the log and checks the file at flush, FUA and mark entries and between them. `kill.py` kills the workload with `SIGKILL` at random points, and with `--lazyfs` it also drops the data that was not synced. See `tests/realdisk/README.md`. The nightly workflow runs them in the `realdisk` job.
- `rupg-crash workload` and `rupg-crash verify` run the T8 workload on a file of the operating system and check a file after a crash against the digests that the workload printed.
- `rupg::check` reports `log_bytes`, the bytes of the log blocks that an open replays, and `rupg check` prints it. The M1 benchmark uses it to show the size of the log before a recovery.

### Changed

- The log checksum uses the CRC32C instruction when the CPU has it: SSE 4.2 on x86-64 and the CRC extension on AArch64. Other CPUs use the table code. The code moved from `rupg-log` to `rupg_kernels::crc32c`.
- Recovery reads each ring one time. The scan finds the safe prefixes and does not copy the block bodies, and the new `Recovery::for_each_safe` gives the safe blocks in ring order to replay and to `rupg check`. An insert into a PAX page moves the bytes with one copy. Spec/11 section 11.16 has the numbers, and spec/24 Q73 tracks the gap to the replay budget.
- An update in the hot store that keeps the length of each value writes the PAX leaf in place, with no decode and encode of the leaf. Recovery replays such updates for each record. A decode of a leaf reads the directory entry of each column once. With `rupg-bench m1 --steps recovery --log-mib 512` on an Apple M-series laptop, the open takes 0.48 s, and it took 0.89 s before. The `file_open` fuzz target runs 275 inputs per second, and it ran 129 before.

## 0.0.10 (2026-10-07)

The tenth patch release on the 0.0 line. It adds the T8 crash test, the M1 fuzz targets and the loom tests, and fixes the two bugs that the fuzz targets found. No crate is published.

### Added

- The T8 crash test in `tests/crash` (spec/21 section 21.9). A seeded M1 workload runs on the simulated disk, and before each sync of the file the test builds crash files from the writes that are not synced: every subset of a small set, and the prefixes, the torn prefixes and a seeded sample of a larger set. A crash file with the same bytes as an earlier one counts once. Each crash file must open with exactly the acknowledged commits, or those and the one commit that was in progress at the cut, and `rupg check` must report no error. `cargo test` runs a short workload, and the `rupg-crash` binary runs a full count, for example `cargo run --release -p rupg-crash -- --files 1000000`. The nightly workflow runs 100,000 crash files.
- `Options::window_pages` in the facade sets the size of the window of the buffer pool. A test that opens many databases sets a small window, because the default reservation of address space takes milliseconds to make and to free.
- `SimIo::read_pieces` in `rupg-platform` gives the bytes of a simulated file in pieces with no copy.
- The M1 fuzz targets `file_open`, `page_decode` and `log_replay` in the separate `fuzz/` workspace (spec/21 section 21.7). See `fuzz/README.md`. The nightly workflow runs each target for 20 minutes.
- loom tests of the latch in `rupg-buffer` and of the reservation of space and group commit in a log ring in `rupg-log` (spec/21 section 21.11). They build with `--cfg loom`, and a CI job runs them on each change: `RUSTFLAGS="--cfg loom" cargo test --release -p rupg-buffer -p rupg-log --lib loom`.

### Changed

- The simulated disk keeps each file in chunks of 64 KiB that a fork and a sync share. A fork or a sync copies no bytes, and a write copies only the chunks that it changes.
- `rupg check` uses a pool of the table form with 64 frames. The table form is fast to make and to free, and its frames are zeroed when it is made, so a small pool makes a check of a small file fast.

### Fixed

- An open of a file of zero bytes with `create` off gave a new empty database. It now gives `58P01`, as for a missing file.
- A child number in an inner page of a tree that pointed to a page at the same level or above made a read or a write loop. Each descent now checks that the level of each inner page is 1 or more and below the level of its parent, and gives `XX001` when it is not. `rupg check` reports an error for such a page.

## 0.0.9 (2026-10-07)

The ninth patch release on the 0.0 line. It adds the `rupg` facade with the M1 Rust API and `rupg check`. No crate is published.

### Added

- The `rupg` facade with the M1 Rust API. `Database::open` opens or makes a `.rupg` file and replays the log when the file was not closed cleanly. `create_table` makes a table with typed columns, and `table` and `table_names` find the tables. A `Transaction` does `insert`, `update`, `delete`, `get`, `scan` and `scan_with` over a range of row ids, and rolls back when it is dropped. `checkpoint` and `close` write a checkpoint, and a transaction that starts when the log is large writes one first. The catalog is one page that holds the tables, the next transaction id and the next OID, and each checkpoint writes it. A missing file with `create` off gives `58P01`, a table name that is in use gives `42P07`, and a name with no table gives `42P01`. Spec/17 section 17.3 has the M1 note.
- `Transaction::scan_from` in `rupg-txn` starts a scan at a row id.
- `rupg check FILE` in the `rupg` binary, and `rupg::check` in the facade. The check opens the file to read and does not replay the log. It checks the header slots, the page table, the free space map, the catalog page, the tree and each row of each table, that each logical page in use belongs to the catalog or to one table, and each block of the log after the redo position. It prints the counts and exits with 0, or prints the error and exits with 1. Spec/21 section 21.9.2 has the M1 note.
- `Tree::check_with` in `rupg-tree`, `HotStore::check_with` in `rupg-hot` and `FileStore::allocated` in `rupg-buffer`, for the check.

### Changed

- `Transactions::checkpoint` in `rupg-txn` takes a function that gives the catalog root and the other fields of the checkpoint. The checkpoint calls it with `W` after it writes the pages.

## 0.0.8 (2026-10-07)

The eighth patch release on the 0.0 line. It makes a commit durable: each commit writes a block to the log, the hot pages go to the file at a checkpoint, and recovery replays the log into the pages of the last checkpoint. No crate is published.

### Added

- Each commit with a change now writes one commit block to the log and returns when the block is safe. `Transactions::with_log` sets the log. The block holds one record for each row that the transaction changed: every column for an insert, the changed columns for an update, and no column for a delete. `replay` applies the safe blocks of a `Recovery` to the tables and gives the newest timestamp and the next transaction id. A record applies only if its commit timestamp is above the `stamp` of its row, so a second replay changes nothing. Spec/11 section 11.16 has the M1 note.
- The hot pages are now in the file. `BufferPool::set_filter` sets a `PageFilter` that changes the copy of each page before the pool writes it, and `BufferPool::flush_all` writes every dirty page and waits for pages that a writer holds. `Transactions::filter_writes` sets the filter that writes only the versions that `visible` sees, so no version that is not committed or not safe goes to the file. `Transactions::checkpoint` stops the changes to the pages, writes them with the versions of one snapshot, moves the redo position of the ring past each block at or below that snapshot and writes the checkpoint. Recovery replays the log into the pages of the last checkpoint. `rewrite_leaf` in `rupg-hot` rewrites the rows of a hot leaf. Spec/11 section 11.15 has the M1 note.

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
