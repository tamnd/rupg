# 5. Compatibility

Written 7 October 2026. Counts are taken at the pin, PostgreSQL `REL_19_STABLE` at `7d3d2db7` (19beta4), unless a line says otherwise.

This document says what "100 percent compatible with PostgreSQL" means for rupg. It gives the five levels and the denominator of each, the rules that compare a rupg answer with the oracle answer, and the bytes that we exclude from the comparison. It gives the size of the PostgreSQL 19 surface and the changes in versions 17, 18 and 19 that a client can see. Then it specifies the version shim for PostgreSQL 14 to 18, the suites that run for each shim version, and the gates by milestone. The levels and the rules come from `../2140/compat/postgres/01-what-compatible-means.md` and `../2140/compat/postgres/02-postgres-19.md`, with rupg in place of rudb. Where rupg differs from that design, this document says so and gives the reason.

## 5.1 What compatible means

A system is compatible when a client cannot tell it from PostgreSQL by the bytes that it receives. We do not use the documentation as the reference. We use a server built from the pin, which we call the oracle. The oracle is correct by definition, also where it disagrees with the documentation. `rupg-compat` runs the oracle and rupg side by side, sends the same input to both and compares the output. Document 21 specifies the harness.

The claim has five levels. Each level has a denominator that we can count at the pin. A level is at 100 percent when every item of its denominator gives the same result on rupg as on the oracle, after the exclusions of section 5.3.

| Level | Name | Denominator at the pin | What 100 percent means |
|---|---|---|---|
| L1 | Connect | 52 message formats (12 frontend and 22 backend message bytes), the 9 trace files of `libpq_pipeline`, the protocol traces of 48 clients | Every trace is byte-identical to the oracle, except the excluded fields |
| L2 | Introspect | 64 catalogs (11 shared), 86 system views, 65 `information_schema` views, every column, and the catalog corpus that a recording proxy captured from the clients | Every row and every column is equal to the oracle for the same schema |
| L3 | Query | 239 regression tests with 290,977 lines of expected output, the differential corpus, the 3,414 rows of `pg_proc` | The same pass set as the oracle |
| L4 | Behave | 135 isolation specs (133 scheduled), Hermitage, an error corpus of 7,112 `ERROR` lines in 191 files, the 438 configuration parameters | The same outcome as the oracle for every permutation and every error |
| L5 | Drop-in | The upstream test suites of the 48 clients in `../2140/compat/postgres/13-clients-and-tools.md` | The same pass rate as against the oracle, per client |

The L5 number is a number per client. We never add the clients into one sum, because a sum hides a client that fails completely behind a client with many tests.

**Drop-in has three conditions.** A client is drop-in compatible when all three are true. First, the only change to the client is the host, the port and the credentials. Second, its L5 suite is at 100 percent. Third, it needs no special client flag. A flag such as Npgsql's `Server Compatibility Mode=NoTypeLoading`, or a setting that turns off DataGrip's JDBC metadata introspection, is a failure of the third condition even when the suite passes with it.

**Clients fail on the catalog first.** The research in `../2140/compat/postgres/` found that clients fail on catalog queries more often than on SQL. A missing column in `pg_type` is a failed connection, not a failed query. So L2 comes before L3 in the milestones. The catalog has two tiers. Tier 1 is `pg_type`, `pg_namespace`, `pg_class`, `pg_attribute`, `version()`, `format_type` and the `reg*` casts. Tier 2 is `pg_index`, `pg_constraint`, `pg_attrdef`, `pg_description`, `pg_enum`, `pg_range`, `information_schema` and `pg_proc`. Tier 1 is part of M2. Tier 2 is part of M3. Document 07 section 7.13 specifies the virtual catalog.

## 5.2 What is compared

`rupg-compat` compares the messages that the client receives. The table gives the rule for each message. A field that the table does not name is compared exactly.

| Message | Compared | Not compared |
|---|---|---|
| `RowDescription` | Every field | The value of a table OID that is not zero. A zero against a non-zero value is a difference |
| `DataRow` | Every byte | Nothing |
| Row order | Exact when the query has an `ORDER BY` that fixes a total order | Otherwise the rows are compared as a multiset |
| `CommandComplete` | The whole tag, for example `INSERT 0 3` | Nothing |
| `ErrorResponse` and `NoticeResponse` | The fields S, V, C, M, D, H, P, p, q, W, s, t, c, d and n | The fields F, L and R |
| `ParameterStatus` | Every reported parameter and its value | The value of `server_version` and of `rupg.version` |
| `BackendKeyData` | The length of the secret key | The process ID and the key bytes |
| `ReadyForQuery` | The status byte | Nothing |
| The message sequence | The order and the type of every message | Nothing |
| COPY | The bytes of all `CopyData` messages, joined | The boundaries between `CopyData` messages |

**Error text is counted.** The primary message, the detail and the hint are part of the comparison. Many drivers and ORMs match on the message text, and some match on the hint. The error corpus of L4 holds 7,112 `ERROR` lines, and each one is a test.

**The routine field.** The F, L and R fields name the C file, the line and the function of the error in PostgreSQL. rupg has no such C code, so these fields are not compared. rupg still sends R for the routines that a known client reads. Rails reads `RevalidateCachedQuery` with SQLSTATE `0A000` to detect a cached plan that is no longer valid, so rupg sends that R value with that error. Document 06 section 6.9 lists the cases.

**The message boundaries of COPY.** PostgreSQL sends one row per `CopyData` message. rupg sends one or more whole rows per message, up to 64 KiB, because the encoder writes vectors (document 06 section 6.11). psql and every driver in the matrix accept both forms, so the comparison joins the data first.

## 5.3 The excluded bytes

This section lists every byte that rupg may send differently from the oracle without a difference being counted. The list is closed. A byte that is not on this list and that differs is a difference. To add an item, you must write the reason in document 24 first, and the item must name a client behavior that does not depend on the byte.

| No. | Excluded | Why a client does not depend on it |
|---|---|---|
| 1 | The shape, costs, row estimates and timings of `EXPLAIN` output | Plans differ by design. Document 15 section 15.9 says which parts of `EXPLAIN` keep the PostgreSQL form, so that tools can parse it |
| 2 | The values of user OIDs | A client reads them and uses them. It does not compare them with constants. The differences between consecutive user OIDs are compared (document 07 section 7.12) |
| 3 | The values of statistics counters | `pg_stat_*` counts depend on the engine. The columns, the types and the presence of rows are compared |
| 4 | The values of physical row identity: `ctid`, `xmin`, `xmax`, `cmin`, `cmax` | The values come from rupg's own storage (document 11 section 11.9). Their types, and the rules that clients rely on, such as "`xmin` changes when the row changes", are compared |
| 5 | Sizes: `pg_relation_size`, `pg_database_size`, `relpages` and similar | rupg stores data in another form, and the cold store is smaller by design |
| 6 | C functions, the C extension ABI and `LOAD` | rupg has no C ABI (document 16 section 16.10). `CREATE FUNCTION ... LANGUAGE C` fails with `0A000` |
| 7 | Physical streaming replication and `pg_basebackup` | rupg replicates with Raft per shard (document 18). Logical replication in and out is counted from M9 |
| 8 | The bytes of the PostgreSQL data directory and `pg_upgrade` | rupg stores one `.rupg` file. The importer of M9 reads a PostgreSQL data directory and a `pg_dump` archive, and the result of the import is compared row by row, but the on-disk bytes are not a goal |
| 9 | Time | `now()`, `clock_timestamp()` and the timestamps in statistics views differ between two runs of the same server |
| 10 | The platform text in `version()` and the source fields of errors | `version()` names rupg and its compiler (section 5.7). The F, L and R fields are in section 5.2 |
| 11 | The internals of `pg_node_tree` and TOAST relations | rupg stores its own expression trees and has no TOAST tables. `reltoastrelid` is 0. The text of `pg_get_expr`, `pg_get_viewdef` and the other deparse functions is compared |
| 12 | The suffix of `server_version` after the version number | The value is `19.0 (rupg X.Y.Z)`. Clients read the leading number. Section 5.7 |
| 13 | The value of the process ID in `BackendKeyData` and `pg_backend_pid()` | rupg sessions are tasks, not processes (document 06 section 6.2). The value is a session number, and the two places show the same number |
| 14 | Server log lines | The log is not part of the protocol. rupg writes the same text for the lines that log analyzers parse, but a difference is not counted |
| 15 | File system paths: `data_directory`, `config_file`, `hba_file`, `ident_file`, `pg_settings.sourcefile` and the `file_name` column of `pg_hba_file_rules` | Two servers on one machine never have the same paths. rupg keeps its host rules inside the file, so these values have another form (document 06 section 6.8) |

Items 1 to 11 are the list in `../2140/compat/postgres/01-what-compatible-means.md` section 1.6. Item 8 is changed, because rupg has an importer and the rudb design did not. Items 12 to 15 are new for rupg. Item 12 follows from section 5.7. Item 13 follows from the session model. Item 14 states a rule that the rudb design had in its harness and not in its list. Item 15 follows from the rule of one file with no sidecar.

**What is never excluded.** The SQLSTATE of an error, the text of an error, the type OIDs of result columns, the typmod of each result column, the column names, the command tag, the status byte of `ReadyForQuery`, the set of reported parameters and the catalog columns. These are the bytes that clients read to decide what to do next.

## 5.4 The PostgreSQL 19 surface

These are the counts that the denominators are built from. Each count was taken from the vendored source at the pin. When the pin moves, every count is taken again in the same change (section 5.12).

| Surface | Count at the pin | Source |
|---|---|---|
| Functions | 3,414, of which 163 aggregates, 15 window functions and 116 set-returning functions | `pg_proc.dat` |
| Types | 197 rows: 114 declared and 83 array types. Of the 114, 26 are pseudo-types, 6 are range types and 6 are multirange types | `pg_type.dat` |
| Operators | 805 | `pg_operator.dat` |
| Casts | 249 | `pg_cast.dat` |
| Access method data | 951 `pg_amop`, 720 `pg_amproc`, 179 `pg_opclass`, 148 `pg_opfamily`, 7 `pg_am` | the `.dat` files |
| Encoding conversions | 98 | `pg_conversion.dat` |
| Configuration parameters | 438: 157 int, 129 bool, 78 string, 42 enum and 32 real | `guc_parameters.dat` |
| SQLSTATEs | 268 in 43 classes | `errcodes.txt` |
| Grammar | 20,077 lines, 3,476 productions, 744 nonterminals, 546 terminals, 6,574 LALR states | `gram.y` |
| Lexer | 1,421 lines | `scan.l` |
| Keywords | 499: 335 unreserved, 78 reserved, 63 column-name and 23 type-or-function-name | `kwlist.h` |
| Catalogs | 64, of which 11 are shared | `src/include/catalog/pg_*.h` |
| System views | 86, of which 50 are `pg_stat*` and `pg_statio*` views | `system_views.sql` |
| `information_schema` views | 65 | `information_schema.sql` |
| Regression tests | 239 scheduled tests, 241 SQL files | `src/test/regress` |
| Isolation specs | 135, of which 133 are scheduled | `src/test/isolation/specs` |
| TAP test files | 294 | `src/test` and `src/bin` |
| contrib | 61 directories, 55 extensions, 22 marked trusted | `contrib/*/*.control` |

**The supported versions.** PostgreSQL supports each major version for five years. PostgreSQL 13 reached end of life on 13 November 2025. PostgreSQL 14 reaches end of life on 12 November 2026. The latest minor versions at the date of this document are 18.6, 17.11, 16.15, 15.19 and 14.24. PostgreSQL 19.0 is not released yet, and the pin is beta 4. The shim covers 14 to 18 because these are the versions that a client in production can expect on 7 October 2026. Version 14 stays in the shim after its end of life for one year, until 12 November 2027, and then it is removed in one change.

## 5.5 Changes by version

These are the changes in 17, 18 and 19 that a client can see. They are the inputs of the shim in section 5.6 and of the work list of the milestones. The 19 list comes from `release-19.sgml` at the pin, which has 3,274 lines and 205 items and is marked "AS OF 2026-09-14".

### 5.5.1 PostgreSQL 17 (26 September 2024)

Direct TLS: a client can start TLS at once, without `SSLRequest`, and the server requires the ALPN protocol `postgresql`. SQL/JSON: `JSON_TABLE` and the constructors and query functions. `MERGE` gets `RETURNING` with `merge_action()`, `WHEN NOT MATCHED BY SOURCE`, and `MERGE` on views. `COPY` gets `ON_ERROR ignore`. Two catalog columns are renamed: `pg_collation.colliculocale` becomes `colllocale`, and `pg_database.daticulocale` becomes `datlocale`. The checkpoint columns move from `pg_stat_bgwriter` to the new view `pg_stat_checkpointer`. The `MAINTAIN` privilege and the role `pg_maintain` are new.

### 5.5.2 PostgreSQL 18 (25 September 2025)

Protocol 3.2 with a cancel key of variable length, and the libpq options `min_protocol_version` and `max_protocol_version`. `search_path` becomes a reported parameter, so the server sends 15 parameters at startup in place of 14. OAuth authentication with the SASL mechanism `OAUTHBEARER` and the setting `oauth_validator_libraries`. MD5 passwords are deprecated. Virtual generated columns are the default, with `attgenerated` 'v'. `NOT NULL` constraints get rows in `pg_constraint`. Constraints can be `NOT ENFORCED`, with the column `conenforced`. `WITHOUT OVERLAPS` and `PERIOD` are new, with the column `conperiod`. `RETURNING` can name `OLD` and `NEW`. New functions `uuidv7`, `uuidv4` and `casefold`, and the builtin collation `PG_UNICODE_FAST`. `EXPLAIN ANALYZE` shows `BUFFERS` by default. In CSV input, `\.` ends the data only when it is alone on its line. New clusters have data checksums on by default.

### 5.5.3 PostgreSQL 19 (beta 4)

New commands: `REPACK` with `CONCURRENTLY`, and `WAIT FOR LSN`. Window functions accept `IGNORE NULLS` and `RESPECT NULLS`. `INSERT ... ON CONFLICT DO SELECT` with `RETURNING`. `GRANT ... GRANTED BY`. `COPY TO` gets `FORMAT json` with `FORCE_ARRAY`, `COPY FROM` gets `ON_ERROR set_null` and a header skip of more than one line, and `COPY TO` works on a partitioned table. `EXPLAIN (ANALYZE, IO)`. New types and functions: `oid8`, casts between `bytea` and `uuid`, `regdatabase`, `tid_block` and `tid_offset`, `random(min, max)` for dates, the encodings `base64url` and `base32hex`, `range_minus_multi`, `error_on_null`, and string methods in `jsonpath`. TLS gets SNI with `ssl_sni` and the file `pg_hosts.conf`. New views `pg_stat_lock`, `pg_stat_recovery`, `pg_stat_autovacuum_scores` and `pg_dsm_registry_allocations`. Several statistics views get a `stats_reset` column, `pg_stats` gets the table OID and the attribute number, and `pg_available_extensions` gets `location`. New settings include `io_min_workers`, `io_max_workers` and `effective_wal_level`.

**Incompatibilities in 19.** These change the answer to an old query, so the shim must know each one.

| Change in 19 | Effect on a client |
|---|---|
| `standard_conforming_strings` is always on, and `escape_string_warning` is removed | `SET standard_conforming_strings = off` fails |
| RADIUS authentication is removed | A `pg_hba` line with `radius` fails to load |
| A login with an MD5 secret sends a warning, controlled by `md5_password_warnings`. `password_expiration_warning_threshold` is new | A `NoticeResponse` after `AuthenticationOk` |
| Names cannot contain a carriage return or a line feed | `CREATE TABLE "a\nb"` fails |
| The default operator class for `inet` and `cidr` is GiST | `CREATE INDEX ... USING gist (inet_col)` works without an opclass |
| `json_array` over zero rows returns `[]` | Not NULL |
| `max_locks_per_transaction` defaults to 128 | `SHOW` gives a new value |
| `jit` is off by default | `SHOW jit` gives `off`. asyncpg reads it |
| The encoding `MULE_INTERNAL` is removed | `CREATE DATABASE ... ENCODING 'MULE_INTERNAL'` fails |
| `COPY FROM ... WHERE` cannot name system columns | An error in place of rows |
| `sync_error_count` is renamed `sync_table_error_count` | A column name in `pg_stat_subscription_stats` |
| `default_toast_compression` defaults to `lz4` | `SHOW` gives a new value |
| The wait event type `BufferPin` is renamed `Buffer` | A value in `pg_stat_activity` |

**Reverted before 19.0.** These features were committed during the 19 cycle and reverted by beta 4. rupg must not implement them as PostgreSQL syntax: SQL/PGQ `GRAPH_TABLE`, online changes of data checksums, `FOR PORTION OF`, `MERGE PARTITIONS` and `SPLIT PARTITIONS`, the functions `pg_get_role_ddl`, `pg_get_tablespace_ddl` and `pg_get_database_ddl`, and a forced `LC_COLLATE` of `C`. `GROUP BY ALL` is also absent. `gram.y` at the pin (20,077 lines) has no `GROUP BY ALL` production. `ALL` after `GROUP BY` parses only as the standard set quantifier before a list of keys. Document 24 holds it as an open item. The property graph syntax of rupg is a rupg addition and is off by default (document 13).

## 5.6 The version shim

### 5.6.1 What the shim is for

Many clients and tools read the server version and change their behavior. Some refuse a version that they do not know. pgAdmin has no query templates for 19 at the date of this document. Rails reloads all of `pg_type` when the server reports 19. Other clients have a test matrix that stops at 17 or 18. A user who moves from PostgreSQL 16 to rupg wants the clients to see 16 until the clients are ready. The shim gives this.

The shim is the setting `rupg.compat_version`. Its values are `14`, `15`, `16`, `17`, `18` and `19`. The default is `19`. The context is `backend`, so a client can set it in the startup packet, and `ALTER ROLE ... SET` and `ALTER DATABASE ... SET` store a default for a role or a database. A change after the session starts fails with `55P02` and the PostgreSQL text `parameter "rupg.compat_version" cannot be set after connection start`. The value cannot change inside a session, because the client has read the version at startup and has made its decisions.

### 5.6.2 What the shim changes

The shim changes what a client reads to decide its behavior, and the small set of behaviors that changed between versions. It does not change the engine. The storage, the transactions, the executor and the optimizer are the same for every shim version. This rule keeps the shim small and keeps the test cost linear in the number of versions.

| Area | What changes for shim version N |
|---|---|
| Version strings | `server_version` is `N.M (rupg X.Y.Z)` where M is the latest minor version of N at the date of the rupg release. `server_version_num` is `N0M` in the PostgreSQL form, for example `160015`. `version()` starts with `PostgreSQL N.M` |
| Reported parameters | The parameters with the `GUC_REPORT` flag in the parameter table of version N. The build reads the set from the vendored table. We expect 13 for 14 and 15 (no `scram_iterations`, which 16 added), 14 for 16 and 17 (no `search_path`), and 15 for 18 and 19. The build checks this expectation |
| Protocol | The highest minor protocol version of N: 3.0 for 14 to 17, 3.2 for 18 and 19 |
| Visible catalog columns | The columns of each catalog and view in version N, in the order of version N. For example, `pg_database.daticulocale` for 15 and 16, `datlocale` for 17 and later |
| Built-in catalog rows | The rows of the `.dat` files of version N for `pg_proc`, `pg_type`, `pg_operator`, `pg_cast` and the others. A function that version N does not have is not in `pg_proc` and is not found by name resolution |
| System views | The views and the view columns of version N, for example no `pg_stat_checkpointer` for 14 to 16 |
| Configuration parameters | The parameter set of version N with the defaults of version N. A parameter that N does not have fails with `42704` as in N |
| Changed behaviors | The switches of section 5.6.3 |
| Authentication | No MD5 warning for 14 to 18. `oauth` lines are refused for 14 to 17 |

**The grammar is not shimmed.** The parser is the 19 grammar for every shim version. A session with shim 14 can run `MERGE ... RETURNING`, which 14 rejects. A client written for 14 does not send it, so this does not break the client. The cost of a grammar for each version is five more parse tables and five more action sets, for no client that we know of. If a client is found that depends on a syntax error of an older version, document 24 records it. This is open question 5.A in section 5.14.

**Function lookup follows the catalog rows.** A name lookup reads the built-in rows of the shim version. This keeps one rule: what `pg_proc` shows is what a call finds. So `uuidv7()` under shim 17 fails with `42883` and the hint of 17, as on PostgreSQL 17.

### 5.6.3 The behavior switches

A behavior switch is a branch in the engine that depends on the shim version. Each switch is a named constant in `rupg-shim`, and each switch has a test in the suite of the version that it serves. We keep the list short. A new switch needs a client or a suite that depends on the old behavior.

| Switch | Versions with the old behavior | Old behavior |
|---|---|---|
| `escape_string_warning` and `standard_conforming_strings = off` | 14 to 18 | The parameters exist and `off` is accepted. With `off`, a plain string literal treats a backslash as an escape |
| `json_array_empty` | 14 to 18 | `json_array` over zero rows returns NULL |
| `inet_default_opclass` | 14 to 18 | The default opclass for `inet` and `cidr` is the B-tree class, and `USING gist` needs `inet_ops` |
| `name_newline` | 14 to 18 | Names can contain a carriage return or a line feed |
| `csv_end_marker` | 14 to 17 | `\.` ends CSV data also when it is not alone on its line |
| `explain_buffers_default` | 14 to 17 | `EXPLAIN ANALYZE` does not show `BUFFERS` unless asked |
| `generated_default_stored` | 14 to 17 | A generated column without `VIRTUAL` or `STORED` must say `STORED`, as the grammar of those versions requires |
| `not_null_constraint_rows` | 14 to 17 | `NOT NULL` has no row in `pg_constraint` |
| `public_create` | 14 | `public` has `CREATE` for `PUBLIC`, and its owner is the bootstrap superuser, not `pg_database_owner` |
| `copy_system_columns_where` | 14 to 18 | `COPY FROM ... WHERE` can name system columns |

The defaults that changed are not switches. They are data in the parameter table of each version, for example `jit`, `max_locks_per_transaction` and `default_toast_compression`. A parameter whose default changed but whose engine meaning is the same needs no code.

Some changes are not shimmed, because the old behavior is a defect that no client needs, or because rupg does not have the mechanism. The removal of `MULE_INTERNAL` is not shimmed: rupg does not implement that encoding for any version. RADIUS is not shimmed: rupg never supports it. The rename of `BufferPin` is data in the view rows.

### 5.6.4 How the shim data is built

The shim data is generated. Nobody writes a catalog row or a column list for an old version by hand.

1. For each version N from 14 to 18, `rupg-compat` holds a pin: the `REL_N_STABLE` commit of the latest minor release. At this document the pins are the commits of 18.6, 17.11, 16.15, 15.19 and 14.24.
2. `cargo xtask pg-vendor-shim N <commit>` copies the catalog headers, the `.dat` files, `system_views.sql`, `information_schema.sql`, `errcodes.txt` and the parameter table of version N into `crates/rupg-shim/vendor/N/`. Branches that do not have `guc_parameters.dat` keep the parameter table in `guc_tables.c`, and the build reads that file for them.
3. The build parses each version with the same code that parses the pin (document 07 section 7.13) and writes the differences against the pin as static tables: columns added, removed, renamed and reordered for each catalog and view, rows added and removed for each `.dat` file, and parameters added, removed and changed.
4. The build checks that every built-in OID that exists in two versions names the same object. If it does not, the build fails, and the difference must be added as a rule by hand with a test.
5. The build writes the version strings and the reported parameter set for each version.

The shim stores differences, not full copies. A full copy of the static catalog for five versions would be about five times the size of the pin's static catalog in the binary. The size of the generated tables is a budget in document 19, and the library build can leave out the shim with a cargo feature.

### 5.6.5 Suites for each shim version

Each shim version has its own oracle and its own suites. The oracle for version N is a server built from the pin of version N in section 5.6.4. `rupg-compat` builds all six oracles in M0 and keeps them in the same harness, so a run can compare rupg under shim N with the oracle of N.

| Suite | 19 (pin) | 18 | 17 | 16 | 15 | 14 |
|---|---|---|---|---|---|---|
| Protocol traces of the 48 clients (L1) | Every commit | Every commit | Nightly | Nightly | Nightly | Nightly |
| `libpq_pipeline` traces (L1) | Every commit | Every commit | Nightly | Nightly | Nightly | Not run, the program is older |
| Catalog suite: `SELECT *` on every catalog and view (L2) | Every commit | Every commit | Every commit | Every commit | Every commit | Every commit |
| Catalog corpus from the recording proxy (L2) | Every commit | Every commit | Nightly | Nightly | Nightly | Nightly |
| Regression suite of the version (L3) | Every commit | Nightly | Weekly | Weekly | Weekly | Weekly |
| Differential corpus (L3) | Every commit | Nightly, the subset that version N can run | Weekly, subset | Weekly, subset | Weekly, subset | Weekly, subset |
| Isolation specs of the version (L4) | Every commit | Nightly | Weekly | Weekly | Weekly | Weekly |
| Hermitage (L4) | Every commit | Not run | Not run | Not run | Not run | Not run |
| Error corpus of the version (L4) | Every commit | Nightly | Weekly | Weekly | Weekly | Weekly |
| Parameter suite: `SHOW ALL`, `pg_settings` and every `SET` (L4) | Every commit | Every commit | Every commit | Every commit | Every commit | Every commit |
| Client suites (L5) | Nightly | Weekly, for clients that test against 18 | Weekly, for clients that test against 17 | Monthly | Monthly | Monthly |

Hermitage runs only against 19, because isolation does not depend on the shim. The engine is the same for every shim version, so a second run would test the same code.

**The regression suite of an older version.** The suite of version N is the `src/test/regress` directory of the pin of N, run through the same `pg_regress` driver. A test in that suite that needs a feature the shim cannot give, for example a C function in `regress.c`, is excluded for the same reason as on 19 (section 5.8). A test whose expected output depends on the 19 grammar accepting what N rejects is a known divergence. It is listed by name in `rupg-compat/shim/N/divergences.toml` with the reason, and the list is published with the number. The list must stay short. A divergence that is not the grammar rule of section 5.6.2 is a defect.

**The shim number.** Each shim version has its own L1 to L5 numbers, published next to the 19 numbers. A shim version is supported when its L1 and L2 numbers are at the level of the 19 numbers. The gates of the milestones (section 5.13) are on the 19 numbers. The shim numbers have one gate, at M9, when the shims become part of the product.

### 5.6.6 What the shim does not do

The shim does not read or write a PostgreSQL data directory of version N. The importer of M9 does that, and it reads the data directories of 14 to 19 (document 17). The shim does not give the bugs of version N. If the oracle of N gives a wrong answer that a later minor or major version fixed, and rupg gives the fixed answer, the run records a difference with the reason "fixed upstream", and the difference is not counted against the shim. The shim does not support a version before 14. Version 13 reached end of life on 13 November 2025, and its protocol and catalog are close to 14, so a client that works with 13 works with shim 14 in almost every case.

## 5.7 Version reporting

A client reads the version in three places: the `server_version` parameter at startup, `SHOW server_version_num`, and `SELECT version()`. All three must say PostgreSQL with a version number first, because clients parse them.

| Place | Value under the default shim |
|---|---|
| `server_version` | `19.0 (rupg X.Y.Z)` |
| `server_version_num` | `190000` |
| `version()` | `PostgreSQL 19.0 (rupg X.Y.Z) on x86_64-pc-linux-gnu, compiled by rustc 1.98.0, 64-bit` |
| `rupg.version` | `X.Y.Z` |

The value of `server_version` uses the form that Debian and Ubuntu packages use, where the version number is followed by a space and a suffix in parentheses. psql, libpq and every driver in the matrix read the leading number and stop at the space. The suffix is excluded from the comparison (section 5.3, item 12). SQLAlchemy parses `version()` with the regular expression `(?:PostgreSQL|EnterpriseDB) (\d+)\.?(\d+)?`, so the text must start with `PostgreSQL` followed by the number. The platform part follows the PostgreSQL form, with the Rust target triple and the compiler.

`rupg.version` is a custom parameter with context `internal`. The server sends it in `ParameterStatus` at startup, after the 15 standard parameters. A client that does not know it ignores it, as the protocol requires. rupg does not send `crdb_version`, `mz_version`, `questdb_version` or any other parameter that a client uses to detect another system, because some clients change their behavior when they see one.

## 5.8 How the regression number is counted

The L3 number has two forms, and we publish both.

**The strict count.** A test passes when `pg_regress` reports it as passed, which means that the output file is equal to the expected file after the standard filters of `pg_regress`. This is the number we use for gates. A test with one wrong line fails.

**The statement count.** The share of statements in the suite whose output is equal to the oracle output. This number shows progress inside a test that fails. It is never used for a gate, because it hides a test that fails on its first statement and stops the rest.

**Exclusions.** A test is excluded from the denominator only when it needs a C function from `regress.c` that rupg does not implement (document 16 section 16.7.6) or a C extension that rupg does not ship, or when it tests an excluded item of section 5.3, for example a test whose purpose is the exact `EXPLAIN` output. The estimate in `../2140/compat/postgres/01-what-compatible-means.md` section 1.7 is that 7 to 10 percent of the tests need `regress.c` functions or C extensions, and that 40 to 50 percent need PL/pgSQL, exact `EXPLAIN` output or the describe output of psql. This is an estimate. The exact list is made at M3 and published. A test that needs PL/pgSQL is not excluded. It waits for M7. A test that prints `EXPLAIN` only to show the plan, while the rest of the test checks results, is run with the `EXPLAIN` lines filtered, and the filtered lines are named in the published list.

**The number can go down.** When the pin moves or a new client enters the matrix, the denominator changes, and the published number can fall. We publish the fall with its cause. We do not hold the pin back to keep a number.

**Publication.** `rupg-compat` publishes one page per commit on `main`. The page has the L1 to L5 numbers for 19, the shim numbers from the last run of each, the list of failing items, and the resource ratios of the same run: peak memory and CPU time of rupg and of the oracle for each suite, measured as document 02 section 2.6 says.

## 5.9 Protocol versions

The server speaks protocol 3.0 and 3.2. Version 3.1 is reserved and libpq never sends it, and the server treats it as a version below 3.2. Version 2.0 was removed from PostgreSQL in 14, and rupg does not speak it. The server answers a request for a minor version above 3.2 with `NegotiateProtocolVersion` and the version 3.2. In 19, PostgreSQL defines a grease version, 3.9999, and a reserved protocol option, `_pq_.test_protocol_negotiation`, to test that servers negotiate correctly. rupg must not have a special case for either value. It must take the normal negotiation path, which gives the right answer. libpq in 19 still requests 3.0 unless the connection string sets `max_protocol_version`, so most sessions use 3.0. CockroachDB had to add 3.2 negotiation in 26.1 when clients began to request it. Document 06 section 6.5 specifies the negotiation.

## 5.10 rupg additions

rupg adds features that PostgreSQL does not have. Each addition must leave the default behavior equal to the oracle, so that the suites stay valid.

| Addition | How it stays out of the comparison |
|---|---|
| `USING link` and `USING vector` indexes (document 13) | New access methods. They appear in `pg_am` only after a user creates the first such index, or when `rupg.show_extra_am` is on. A fresh database has the 7 access methods of the pin. The pgvector names `hnsw` and `ivfflat` appear when the `vector` extension is installed, as in PostgreSQL with pgvector |
| `USING columnar` and the storage parameters of document 10 | Accepted as hints. `columnar` appears in `pg_am` only after it is used |
| Property graph syntax (document 13) | Off by default. The lexer does not return its keywords unless `rupg.graph_syntax` is on |
| `rupg.*` settings | Custom parameters with a prefix that PostgreSQL does not use. `SHOW ALL` and `pg_settings` list them after the PostgreSQL parameters only when `rupg.show_extra_settings` is on. The default is off, so `SHOW ALL` gives the 432 rows of PostgreSQL to a superuser |
| `rupg.*` functions | In the schema `rupg`, which is not in the default search path. `pg_proc` lists them, because they are real functions in a real schema. The catalog suite filters the schema `rupg` |

**Why the extras are hidden.** A GUI that lists access methods, settings or schemas shows what it finds. If rupg showed its extras by default, the L2 comparison would fail on every fresh database for no client benefit. A user who wants to see them turns on one setting.

## 5.11 Extensions and the count

The extensions that rupg implements natively are listed in document 16 section 16.8. An extension counts toward L3 when the regression tests of its `contrib` directory pass against rupg as against the oracle with the same extension installed. The pgvector suite counts the same way against an oracle with pgvector built from its pinned release. An extension that rupg does not implement fails `CREATE EXTENSION` with the PostgreSQL error for a missing control file, and it is not in any denominator. A C extension is never counted (section 5.3, item 6). The published page lists the extensions with their own pass rates.

## 5.12 How the pin moves

The pin moves on a branch and lands on one day. The steps are these.

1. Make a branch. Rebuild the oracle at the new commit.
2. Run `cargo xtask pg-vendor <commit>`. It copies `gram.y`, `scan.l`, `kwlist.h`, the catalog headers and `.dat` files, `guc_parameters.dat`, `errcodes.txt`, the system view scripts and the regression, isolation and TAP suites, and records the SHA-256 of each file.
3. Fix the generated tables until the build passes. A new grammar conflict or a production with no action fails the build (document 07 section 7.2).
4. Run every suite. Write the changes to every denominator in the commit message: tests added and removed, catalog columns added and removed, parameters added and removed.
5. Update every count in this folder that the change touches.
6. Merge on the same day. Do not leave `main` between two pins.

When 19.0 is tagged, on or after 29 October 2026, the pin moves to the tag in this way. Each minor release of 19 moves the pin in the same way. The shim pins move when a new minor release of an older version comes out, with the same steps and `pg-vendor-shim`.

## 5.13 Gates by milestone

This table gives the compatibility condition of each milestone. Document 23 owns the gates. The numbers below are the compatibility part of them.

| Milestone | Compatibility condition |
|---|---|
| M0 | The six oracles build and run. `rupg-compat` runs every suite against the oracle of 19 and against itself, and gets 100 percent, which tests the harness |
| M1 | No PostgreSQL surface. The Rust KV-level API has its own tests |
| M2, gate G1 | L1: psql, libpq and the 14 drivers of the connect matrix connect, authenticate with SCRAM over TLS, run a simple query and disconnect, with byte-identical traces. L2 tier 1 at 100 percent |
| M3, gate G4 | L2 tier 2 at 100 percent. L3: the regression tests that need no PL/pgSQL, no transaction feature of M5 and no index type of M6 pass. The list is fixed at the start of M3 |
| M4, gates G2 and G3 | `COPY` in every format of the pin, compared with the oracle. No regression from M3 |
| M5, gate G5 | L4: the 133 scheduled isolation specs and Hermitage at 100 percent |
| M6 | The index tests of the regression suite pass. The `contrib` tests of `btree_gin`, `btree_gist` and `pg_trgm` pass |
| M7, gate G6 | L3: the full regression suite at 100 percent of its denominator, including PL/pgSQL, triggers and rules |
| M8, gates G7 to G10 | No regression in L1 to L4 while the performance work lands |
| M9 | Shims 14 to 18: L1 and L2 at the level of 19. The importer reads a `pg_dump` of the regression database of each version and the result matches the oracle row by row |
| M10 | No regression. Failover keeps sessions' observable behavior as document 18 says |
| M11, gate G11 | No regression on a cluster of three nodes: every L3 and L4 suite also runs against a sharded database |
| M12, gate G12 | L1 to L5 at 100 percent for 19, for every client in the matrix |

## 5.14 Open questions from this document

5.A. Should a session under an older shim refuse syntax that the older version did not have? Section 5.6.2 says no. A client that depends on such a syntax error would change the answer.

5.B. Closed. `gram.y` at the pin has no `GROUP BY ALL` production, so rupg does not add one. If PostgreSQL 20 adds it, it comes with the pin move of section 5.12.

5.C. The size of the shim tables in the binary is not measured. If it is too large for the library or WebAssembly builds, the shim must load from a separate data blob, which conflicts with the rule of one binary and one file.

5.D. Version 14 leaves upstream support on 12 November 2026. Section 5.4 keeps it in the shim for one more year. The date to drop it depends on the client matrix, which is not measured yet.
