# 6. Wire and sessions

Written 7 October 2026. Counts are taken at the pin, PostgreSQL `REL_19_STABLE` at `7d3d2db7` (19beta4).

This document specifies how a client reaches rupg over the network. It covers the protocol codec, the server runtime with one worker per core, startup and negotiation, TLS, authentication and the host rules, the simple and extended query flows, results and backpressure, `COPY`, cancel and timeouts, the configuration parameters, pooling, shutdown, and the gates. The protocol facts come from `../2140/compat/postgres/04-the-wire-protocol.md`, `05-the-server.md` and `09-sessions-and-settings.md`, which read the pin line by line. This document does not repeat every message format. It states what rupg does and where rupg differs from the rudb design. The main difference is the runtime: the rudb design used a blocking thread per connection, and rupg runs each session as a task on a worker with `io_uring` or `kqueue`.

## 6.1 The crates and the boundary

Three crates own this document.

| Crate | Rank | Owns |
|---|---|---|
| `rupg-wire` | 1 | The protocol codec. It turns bytes into frontend events and backend messages into bytes. It does no I/O, holds no socket and knows no engine type. It holds the SCRAM state machine and the cancel key rules |
| `rupg-session` | 12 | The session: its state, its settings, its prepared statements and portals, the plan cache lookup, the transaction status, the reset commands and the parameter reporting |
| `rupg-server` | 13 | The runtime glue: listeners, TLS, the host rules, the cancel registry, the timers, shutdown and the process entry point |

The codec has no I/O so that it can be tested with byte arrays, fuzzed, and used by the C API and WebAssembly builds (document 17). The codec is the same code that the rudb design built in `rudb-pgwire`, lifted as document 04 section 4.9 allows, with the crate renamed. `rupg-session` sits at rank 12 because it calls the planner and the executor. `rupg-server` is thin. A program that embeds rupg can run the same sessions over its own transport with `rupg-session` and `rupg-wire` alone.

## 6.2 The runtime: workers, tasks and sockets

**One worker for each core.** The server starts one worker thread for each core and pins it on Linux. Each worker runs the scheduler of document 04 section 4.5 with its four priority queues. Sessions, statements, morsels of analytic queries and background work all run on these workers. There is no other pool of threads in the server, except a small pool for blocking file calls on macOS and the timer, which section 6.12 describes.

**A session is a task.** A session is a Rust future that the scheduler polls. It waits for input, runs one statement at a time, and writes output. When it waits for the socket, for a lock or for a morsel result, it yields, and the worker runs other work. A short statement runs in queue 1. An analytic statement splits into morsels in queue 2, and the morsels run on every worker. So 1,000 sessions do not need 1,000 threads, and a long query does not block the sessions that share its worker.

**Sockets on Linux.** Each worker owns one `io_uring` instance. A session socket is registered on the ring of one worker, its home. The home arms a multishot receive that takes buffers from a ring of provided buffers that the worker owns. A receive completion gives the session a buffer with the bytes that arrived. The session feeds the codec, and the codec returns the buffer to the ring when it has parsed the bytes. If a message is not complete, the codec copies the partial message into a buffer that the session owns until the rest arrives. So an idle session holds no receive buffer. Writes are send operations from the output buffer of the session. On Linux 6.0 and later, sends of large results use zero-copy send when the result is larger than a threshold that `rupg-bench` sets.

**Sockets on macOS.** Each worker owns one `kqueue`. A session socket is registered for read and write readiness on its home worker. A readable event reads into a scratch buffer of the worker, and the codec copies only partial messages, as on Linux. The macOS path is tier 2, and it is tested for correctness and for the memory gate of section 6.4, not for the throughput gates.

**Windows and WebAssembly.** Windows uses I/O completion ports from M9. The WebAssembly build has no server. A program in the browser uses the library API, or a protocol channel over a message port that runs the same `rupg-session` code (document 17).

**Accepting connections.** Each listening address has one socket. The first worker arms a multishot accept on it. A new connection is given to the worker with the fewest live sessions, which becomes its home. The server does not use `SO_REUSEPORT` to spread connections, because it does not work for Unix sockets on Linux and does not spread connections on macOS, and one accepting ring is enough for the connection rate target of section 6.18. A worker that runs the startup of a session runs the TLS handshake and the authentication as normal tasks, so a slow client does not block the accept.

**A session can move.** A session keeps its home between statements. The scheduler can move it to another worker at the start of a statement when the home is overloaded. The move cancels the receive on the old ring and arms it on the new ring. The move never happens while a statement runs, so the order of messages on the socket cannot change.

**Backpressure.** Three rules keep memory bounded. First, a session reads no new message while its output buffer holds more than the flush threshold, so a client that sends a large pipeline and does not read cannot make the server buffer without limit. Second, a statement that produces rows yields when the output buffer is over the threshold and the socket cannot take more, and the morsels of that statement are not scheduled until the socket drains. Third, every buffer that a session allocates is charged to `rupg.memory_limit` (document 04 section 4.6). When the budget refuses a charge, the session waits, and it is not killed. PostgreSQL also stops reading when its output is blocked, so a client sees the same behavior.

## 6.3 The life of a session

A session goes through these states. The codec owns the states before `Ready`. The session owns the rest.

| State | What happens | Next |
|---|---|---|
| `Accepted` | The socket is assigned to a home worker | `Startup` |
| `Startup` | The codec reads `SSLRequest`, `GSSENCRequest`, `CancelRequest` or `StartupMessage`. TLS starts here (section 6.6) | `Authenticating`, or closed for a cancel |
| `Authenticating` | The host rules choose a method. The method runs (section 6.7) | `Ready` or closed |
| `Ready` | The server sends `AuthenticationOk`, the MD5 warning if one applies, one `ParameterStatus` for each reported parameter, `rupg.version`, `BackendKeyData` and `ReadyForQuery` with `I` | `Idle` |
| `Idle` | No statement runs. The session waits for input or a notification | `Running` |
| `Running` | A statement runs. The session yields while it waits | `Idle` |
| `Copy` | A `COPY FROM STDIN` or `COPY TO STDOUT` is in progress (section 6.11) | `Running` |
| `Closing` | Rollback of an open transaction, release of locks, removal from the registries | closed |

The server sends the reported parameters in the order that the oracle used at the pin. `rupg-compat` records that order once for each pin and each shim version, so a trace diff stays clean. The order has no meaning to clients.

**The session number.** PostgreSQL gives the process ID in `BackendKeyData` and `pg_backend_pid()`. rupg has no process for each session. It gives a session number, a 32-bit value that is unique among live sessions. The first number at server start is random and above 1,000, and each new session takes the next free value. The same number is in `pg_stat_activity.pid` and is the argument of `pg_cancel_backend` and `pg_terminate_backend`. A tool that passes the number to an operating system command does not work. That tool needs a shell on the server host, so this is acceptable. Document 05 section 5.3 excludes the value from the comparison.

**The temporary schema.** A session that creates a temporary object gets the schema `pg_temp_N`, where N is the lowest free slot from 1 to `max_connections`. The slot is taken at the first temporary object, not at connect. Document 07 section 7.13 shows these schemas in the catalog.

## 6.4 The idle session costs at most 64 KiB

### 6.4.1 The rule

An idle session must cost at most 64 KiB of memory on the server. This is the gate of document 02 section 2.6.2. It is measured as `../2140/compat/postgres/15-performance.md` says: open 1,000 idle connections, take the increase in the total proportional set size of the server process, and divide by 1,000. The same measurement runs against the oracle on the same machine, and both numbers are published. An idle session is a session in state `Idle` that has completed startup and has run no statement, or that has run `DISCARD ALL`. The kernel memory of the socket is not in the process size. It is measured separately from the cgroup and published next to the session number, because the cgroup counts it in the peak memory of document 02.

### 6.4.2 The breakdown

The table is a budget, not a measurement. Each line has an owner who keeps it, and the test in section 6.4.3 fails when the total is over 64 KiB.

| Part | Budget | How it is kept small |
|---|---|---|
| Session state: identity, roles, database handle, session number, cancel key, status flags, timer handles, transaction status | 1 KiB | Fixed-size fields. The database handle and the role are shared references |
| Settings | 2 KiB | A session holds only the values that differ from a shared base. The base is the defaults for the database and the role, shared by all sessions with the same pair. Section 6.13 |
| Activity slot: the row of `pg_stat_activity`, with the query text of `track_activity_query_size`, which is 1,024 bytes by default | 2 KiB | One slot in a shared table. It grows only when the setting grows |
| The task frame of the idle future | 2 KiB | The idle future waits on one receive. The frame of a running statement is freed when the statement ends |
| Receive buffer | 0 KiB | Idle sessions hold no receive buffer on Linux (section 6.2). A partial message buffer is freed when the message is complete |
| Output buffer | 4 KiB | The buffer shrinks to 4 KiB when the session goes idle. A buffer that grew above 1 MiB is freed |
| Prepared statement and portal tables | 1 KiB | Empty tables. A prepared statement holds a name and a shared reference to a plan in the plan cache of the node, so it is counted separately, in bytes per statement |
| `LISTEN` channels, advisory locks, cursors | 0.5 KiB | Empty sets |
| TLS state | 24 KiB | The rustls connection with its record buffers, shrunk after the handshake. A plain connection has 0 |
| Allocator overhead | 8 KiB | Size classes and alignment |
| Reserve | 19.5 KiB | Not allocated. It is the margin for the parts that measurement shows to be larger |
| Total | 64 KiB | |

**What is not in the session.** PostgreSQL gives each backend its own catalog cache, relation cache, plan cache and work memory. rupg shares all of them across sessions. The catalog cache and the plan cache are one structure for the node, keyed by the catalog version (document 07 section 7.11). Operator memory is reserved from the node budget for the length of a statement and returned at its end. This is the reason that an idle rupg session can be small, and it is also why a rupg session that has run a statement does not keep the memory of that statement.

### 6.4.3 The test

`rupg-bench` runs the measurement of section 6.4.1 on Linux and macOS on every commit to `main` that touches `rupg-wire`, `rupg-session` or `rupg-server`, and nightly otherwise. It runs four variants: plain over a Unix socket, plain over TCP, TLS over TCP, and TLS with SCRAM after `DISCARD ALL`. The gate is the largest of the four. A debug assertion in `rupg-session` also adds the sizes of the parts above for each idle session, from the allocator, and fails a test when a part is over its budget. The TLS line is the most uncertain, because the size of the rustls state after the handshake is not measured yet. If it is above 24 KiB, the first remedy is to move the record layer into the kernel with kTLS on Linux, which document 24 records as open question 6.B.

## 6.5 Startup and protocol negotiation

A connection starts with one of four packets. Each packet has a 32-bit length and a 32-bit code. The codes and the answers are those of PostgreSQL, from `pqcomm.h` at the pin.

| Code | Packet | Answer |
|---|---|---|
| 80877103 (1234.5679) | `SSLRequest` | One byte: `S` if TLS is on, `N` if not |
| 80877104 (1234.5680) | `GSSENCRequest` | One byte: `N`. rupg has no GSSAPI encryption |
| 80877102 (1234.5678) | `CancelRequest` | No answer. Section 6.12 |
| 3.0 or 3.2 | `StartupMessage` | Authentication and the rest of startup |

The startup packet may be at most 10,000 bytes. A longer packet closes the connection. The server reads exactly the 8 bytes of `SSLRequest` and does not read ahead. If the buffer holds more bytes after `SSLRequest`, the server fails with `08P01` and `received unencrypted data after SSL request`, as PostgreSQL does, because those bytes could be injected before TLS starts.

**Direct TLS.** A connection whose first byte is `0x16` starts a TLS handshake at once, with no `SSLRequest`. The server requires the ALPN protocol `postgresql`. If the client does not offer it, the server closes the connection. After the handshake, the client sends a normal `StartupMessage` inside TLS. libpq uses this with `sslnegotiation=direct`, which PostgreSQL 17 added.

**The version.** The high 16 bits are the major version and the low 16 bits are the minor version. The server accepts major version 3 only. For major version 1 or 2, it sends the error in the old version 2 form, with no length word, as PostgreSQL does. For any other major version, it sends `0A000` and `unsupported frontend protocol %u.%u: server supports %u.0 to %u.%u`. For a minor version, the session uses the lower of the request and 3.2. Version 3.1 is reserved, and the server treats it as a version below 3.2.

**Negotiation.** After it reads the parameters, the server sends `NegotiateProtocolVersion` if the requested minor version is above 3.2 or if the packet has a protocol option that starts with `_pq_.` and that the server does not know. The message holds the version of the session and the names of the unknown options. rupg knows no protocol options at the pin, so every `_pq_.` name is unknown. The grease version 3.9999 and the grease option `_pq_.test_protocol_negotiation` take this path with no special case. A request for 3.9999 gets 3.2. The server does not wait for an answer.

**The parameters.** `user` is required. If it is missing or empty, the error is `28000` with `no PostgreSQL user name specified in startup packet`. `database` defaults to the user name. `replication` selects a replication session (section 6.16). `options` holds command-line style settings. Every other name is a configuration parameter, applied as `SET` with source `client` (section 6.13). If the same name comes twice, the last value wins. An invalid value fails startup with `22023`.

**The cancel key.** Under 3.0 the key in `BackendKeyData` is 4 random bytes. Under 3.2 it is 32 random bytes, which is `MAX_CANCEL_KEY_LENGTH` at the pin. libpq in 19 requests 3.0 unless `max_protocol_version` is set, so most keys are 4 bytes.

**Under a shim.** The shim version caps the protocol version. A session with `rupg.compat_version` 14 to 17 in its startup packet, or as a default for its role or database, uses 3.0 and a 4-byte key, as those versions do (document 05 section 5.6.2). The role default is known only after the role is found, which is before the server sends `NegotiateProtocolVersion`, so the order of the messages does not change.

## 6.6 TLS

**The library.** TLS uses rustls with the aws-lc-rs provider, as `rudb-server` did. The same provider gives SHA-256, HMAC and PBKDF2 to SCRAM, so the build has one implementation of each primitive. TLS 1.2 and 1.3 are on. `ssl_min_protocol_version` and `ssl_max_protocol_version` are honored with the PostgreSQL defaults. `ssl_ciphers` is honored for TLS 1.2 and accepted for TLS 1.3, where PostgreSQL also uses `ssl_tls13_ciphers`.

**Certificates and keys.** The server reads `ssl_cert_file`, `ssl_key_file`, `ssl_ca_file` and `ssl_crl_file` from paths, as PostgreSQL does. They are server configuration, not data, so they are not stored in the `.rupg` file. A program that embeds the server can pass them as bytes through the API. A reload with `pg_reload_conf()` loads new files for new connections.

**SNI.** PostgreSQL 19 adds `ssl_sni` and `pg_hosts.conf`, which choose a certificate by the host name that the client sends. rupg implements both at M9. Before M9, `ssl_sni` is accepted and must be `off`. An attempt to set it to `on` fails with `0A000`.

**What TLS changes in a session.** A TLS session offers `SCRAM-SHA-256-PLUS` with the channel binding type `tls-server-end-point`, which is the only type PostgreSQL supports. `pg_stat_ssl` shows the version, the cipher and the client certificate fields of each session. `hostssl` rules match only TLS sessions.

## 6.7 Authentication

The server sends an `AuthenticationRequest` with a code, and the client answers with a `p` message. rupg supports these methods.

| Method | Codes | Milestone | Notes |
|---|---|---|---|
| `trust` | 0 | M2 | |
| `reject` | none | M2 | The error is `28000` |
| `scram-sha-256` | 10, 11, 12 | M2 | The default. `-PLUS` on every TLS session |
| `md5` | 5 | M2 | Uses SCRAM when the stored secret is SCRAM, as PostgreSQL does. Sends the 19 warning |
| `password` | 3 | M2 | Cleartext. The default rules allow it only on TLS |
| `peer` | none | M2 | Unix sockets only. Uses `SO_PEERCRED` on Linux and `getpeereid` on macOS |
| `cert` | none | M2 | The common name of the client certificate, with the maps of `pg_ident` |
| `ldap`, `pam` | 3 | M12 | |
| `oauth` | 10 | M9 | `OAUTHBEARER`, with a validator written against the extension API of document 16 |
| `gss`, `sspi` | 7, 8, 9 | Not planned | A `pg_hba` line with them fails to load |
| `radius` | none | Never | Removed in PostgreSQL 19 |

**SCRAM.** The exchange follows RFC 5802 and RFC 7677 and the order of checks in `auth-scram.c`. A client that sends bad SCRAM bytes gets the error of the first check that fails, with the same SQLSTATE and text as the oracle. The codec tests hold one case for each check, taken from `../2140/compat/postgres/04-the-wire-protocol.md` section 4.4. The iteration count comes from the stored secret. `scram_iterations` defaults to 4096 and is a reported parameter. SASLprep is applied to a cleartext password before it is checked against a SCRAM secret and before a new secret is made. The rudb design did not have SASLprep, and rupg must have it at M2, because a password with characters outside ASCII otherwise fails on rupg and passes on PostgreSQL.

**A role that does not exist.** The server does not tell the client that a role does not exist or has no password. It runs a full SCRAM exchange with a mock secret. The salt of the mock secret is derived from the role name and a random nonce that is made once when the file is created and stored in it. The failure is the same `28P01` and `password authentication failed for user "%s"` as for a wrong password, after the same work. So the time of the answer does not show that the role is missing.

**MD5.** A login with an MD5 secret sends a `NoticeResponse` with `WARNING`, code `01000` and `authenticated with an MD5-encrypted password` right after `AuthenticationOk`, as PostgreSQL 19 does, unless `md5_password_warnings` is off. A session under shim 14 to 18 does not get the warning. `password_expiration_warning_threshold` is honored from M2, with the text of the pin.

**Where secrets live.** Roles and their secrets are rows of `pg_authid` inside the file. A secret is stored in the PostgreSQL form `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`, or as an MD5 secret when `password_encryption` is `md5`. The server never stores a cleartext password. A client that sends a secret that is already hashed, as psql's `\password` does, has it stored as given.

## 6.8 Host rules inside the file

PostgreSQL reads its host rules from `pg_hba.conf` and its user maps from `pg_ident.conf`. A `.rupg` file has no sidecar, so rupg keeps both inside the file, in two shared catalog tables of rupg: `rupg_hba` and `rupg_ident`. The text format is the PostgreSQL format, with the same keywords, the same connection types (`local`, `host`, `hostssl`, `hostnossl`, `hostgssenc`, `hostnogssenc`), the same address forms, the same options and the same `include` rules except that an include names another stored text, not a path.

**How the rules change.** The function `rupg.set_hba(text)` takes the full text of the rules, parses it with the PostgreSQL parser rules, and stores it in one transaction. If a line has an error, the function fails with the file name `pg_hba.conf`, the line number and the PostgreSQL text, and nothing changes. `rupg.set_ident(text)` does the same for the user maps. The new rules apply after `pg_reload_conf()` or `SIGHUP`, as in PostgreSQL. Only a superuser can call these functions.

**A file on disk wins.** An operator who manages configuration as files can set `hba_file` and `ident_file` on the command line or in a configuration file. Then the server reads those files and ignores the stored rules, and `pg_hba_file_rules` shows the file path. When the rules are stored, `hba_file` shows the path of the `.rupg` file followed by `#pg_hba.conf`. Document 05 section 5.3 excludes paths from the comparison.

**The view.** `pg_hba_file_rules` and `pg_ident_file_mappings` have the columns of the pin and show the rules that the next reload will use, with the `error` column for a bad line. Tools read these views to check a change before a reload.

**The default rules.** When a server creates a new file with a superuser password, the stored rules are `local all all scram-sha-256` and `host all all` for `127.0.0.1/32` and `::1/128` with `scram-sha-256`. Without a password, the rules are `local all all trust` only, as `initdb --auth-local=trust` gives. `listen_addresses` defaults to `localhost`, as in PostgreSQL.

**In process.** The library API has no host rules. A program that opens the file has the file, so it can read every byte, and a rule would protect nothing. A program that wants authentication in process uses the server crate over a socket pair.

When no rule matches, the error is `28000` with `no pg_hba.conf entry for host "%s", user "%s", database "%s", %s`, with the PostgreSQL text even when the rules are stored in the file. Clients and operators search for this text.

## 6.9 Queries

### 6.9.1 The simple and the extended flow

The simple flow is one `Query` message with one or more statements. The server runs them in order and sends the results, then `ReadyForQuery`. Several statements in one message run in one implicit transaction unless they contain transaction control.

The extended flow is `Parse`, `Bind`, `Describe`, `Execute`, `Close`, `Flush` and `Sync`. A named prepared statement lives until `Close` or the end of the session. The unnamed statement and the unnamed portal live until the next `Parse` or `Bind` of the unnamed object. `Execute` with a row limit returns `PortalSuspended` and keeps the portal open. A portal outside a transaction block lives until the next `Sync`.

### 6.9.2 The plan cache

The plan cache is one structure for the node, as document 04 section 4.2 says. The key is the statement text, the parameter types, the effective `search_path`, the role when row security applies, the shim version and the catalog version. A prepared statement in a session holds a name and a reference to the cache entry. Two sessions that prepare the same text share one plan. The simple flow uses the same cache, keyed by the whole message text. A plan is valid while the catalog version is the same. After DDL, the next use plans again.

PostgreSQL chooses between a custom plan for each set of parameter values and a generic plan, with `plan_cache_mode`. rupg honors the setting. With the default `auto`, rupg uses the generic plan unless the optimizer marks a parameter as one whose value changes the plan, for example a parameter in a range predicate on a column with skewed statistics. Document 15 owns the rule. The point path of document 14 section 14.9 never builds a plan tree for the statements it serves.

**A plan that can no longer give its result type.** When DDL changes the result type of a prepared statement, PostgreSQL fails the next `Execute` with `0A000` and `cached plan must not change result type`, and sets the routine field R to `RevalidateCachedQuery`. pgjdbc and Rails read this field to decide to prepare the statement again. rupg sends the same error with the same R value. This is the one routine name that rupg sends at M3. The recording proxy of `rupg-compat` logs every R value that a client in the matrix reads, and each one that appears is added here with its error.

### 6.9.3 Errors and the skip state

In the simple flow, an error ends the current `Query` message. The server sends `ErrorResponse` and then `ReadyForQuery`. In the extended flow, an error in `Parse`, `Bind`, `Describe`, `Execute`, `Close` or `Flush` puts the session in a skip state. The server reads and drops every message until `Sync`, and then sends `ReadyForQuery`. The codec owns the skip state, so the session never sees the dropped messages. While it skips, the codec still checks each type byte, so a bad type byte is still `FATAL`.

In a failed transaction block, each statement except `COMMIT`, `ROLLBACK` and `ROLLBACK TO SAVEPOINT` fails with `25P02` and `current transaction is aborted, commands ignored until end of transaction block`. `COMMIT` in this state rolls back and returns the tag `ROLLBACK`. The status byte of `ReadyForQuery` is `I`, `T` or `E`. PgBouncer in transaction mode reads it to return a server connection to its pool, so a wrong byte corrupts a pool. `rupg-compat` checks the byte after every statement of every suite.

The codec checks each string field for the client encoding when it reads that field, before it reads the next one, as PostgreSQL does. So a `Query` with an invalid UTF-8 byte and trailing garbage fails with `22021`, not `08P01`.

### 6.9.4 Pipelining

A pipeline is a series of extended messages that the client sends without waiting. The server needs no mode for it. It must read in order, answer in order, not wait for the client between messages, flush at `Sync`, at `Flush` and when the output buffer passes the threshold, and skip to `Sync` after an error. A pipeline with one `Sync` at the end is one implicit transaction, so an error in the fifth statement rolls back the first four. One receive can hold many messages, and the session handles all of them before it writes. So a pipeline of 100 `Bind` and `Execute` pairs costs one receive and one send. The 9 trace files of `libpq_pipeline` are the test, compared byte for byte.

## 6.10 Results and backpressure

The executor gives results as vectors of 1,024 values for each column. The encoder in `rupg-session` writes `DataRow` messages from the vectors. It works one column at a time in two passes: first it computes the encoded length of every value of each column, then it adds the lengths across columns and takes a prefix sum to find the offset of each row, reserves the output once, writes the row headers, and writes each column into its slots. So the type branch happens once for each vector and not once for each value. A dictionary-coded string column is encoded once for each dictionary entry and copied for each row. NULLs come from the validity mask. The design is `../2140/compat/postgres/06-types-and-oids.md` section 6.15, lifted to rupg. The text and binary output rules for each type are in document 07 section 7.9.

The session flushes at `ReadyForQuery`, at `Flush`, at `PortalSuspended` and when the output buffer passes 64 KiB. It does not flush after each `DataRow`, because each flush is a system call. When the socket cannot take more data, the statement yields as section 6.2 says. A result of 100 million rows uses one output buffer of memory, not the size of the result.

**RowDescription.** Each result column carries the name, the table OID and column number of its source column, the type OID, the type length, the typmod and the format. The analyzer carries the source table and column of each output expression to this point, because pgjdbc, sqlx and DBeaver read them (document 07 section 7.5).

## 6.11 COPY

`COPY FROM STDIN` and `COPY TO STDOUT` use the copy sub-protocol: `CopyInResponse` or `CopyOutResponse`, `CopyData` messages, then `CopyDone` or `CopyFail`. The formats are those of the pin.

| Format | Direction | Notes |
|---|---|---|
| `text` | in and out | Tabs, `\N` for NULL, backslash escapes |
| `csv` | in and out | Every option of the pin, including `HEADER` with more than one line to skip, `FORCE_QUOTE`, `FORCE_NOT_NULL`, `FORCE_NULL`. `\.` ends the data only when it is alone on its line, except under shim 14 to 17 |
| `binary` | in and out | The 11-byte signature, the flags, the header extension and the binary values of document 07 section 7.9 |
| `json` | out | `FORMAT json` with `FORCE_ARRAY`, new in 19 |

`ON_ERROR ignore` and `ON_ERROR set_null` are supported on input, with `LOG_VERBOSITY` and the notices that the pin sends. `COPY TO` from a partitioned table and from a query are supported.

**The bulk path.** `COPY FROM STDIN` parses `CopyData` into column vectors and hands each full vector to the bulk load path of `rupg-table`, which writes it to the cold store when the table is empty or the load is large, and to the hot store otherwise (document 10). It does not insert one row at a time. Row triggers and foreign keys still run for each row in the order PostgreSQL gives, as document 16 section 16.4 says. A `COPY` into a table with row triggers is slower than one without, as in PostgreSQL.

**Output.** `COPY TO STDOUT` uses the encoder of section 6.10. It sends one or more whole rows per `CopyData` message, up to 64 KiB. Document 05 section 5.2 compares the joined data, not the boundaries.

In the extended flow, the server ignores `Flush` and `Sync` that arrive during a copy. Another message type during `COPY FROM STDIN` is `08P01` and then a `FATAL` that the protocol synchronization was lost, with the texts of the pin.

## 6.12 Cancel, timeouts and asynchronous messages

**The cancel registry.** The server keeps a map from session number to the cancel key and a handle to the session task. A `CancelRequest` arrives on a new connection. The worker that accepts it looks up the number, compares the key in constant time and, on a match, sets the cancel flag of the session and wakes it. A key matches only when it has the same length and the same bytes. Session number 0 never matches. The server sends nothing back and closes the new connection. The statement fails with `57014` and `canceling statement due to user request` at its next check. A running statement checks the flag between vectors and between morsels, so the delay is about one morsel, which document 04 section 4.5 sizes at about 1 ms.

`pg_cancel_backend` and `pg_terminate_backend` use the same registry. Terminate sends `FATAL` `57P01` with `terminating connection due to administrator command` and closes the session.

**Timeouts.** One timer wheel on each worker holds the deadlines of the sessions whose home is that worker. A session that moves takes its deadlines with it.

| Setting | On expiry | SQLSTATE |
|---|---|---|
| `statement_timeout` | `canceling statement due to statement timeout` | `57014` |
| `lock_timeout` | `canceling statement due to lock timeout` | `55P03` |
| `transaction_timeout` | `terminating connection due to transaction timeout`, FATAL | `25P04` |
| `idle_in_transaction_session_timeout` | `terminating connection due to idle-in-transaction timeout`, FATAL | `25P03` |
| `idle_session_timeout` | `terminating connection due to idle-session timeout`, FATAL | `57P05` |

**Asynchronous messages.** The server can send three messages when the client is not in the middle of a message. `NotificationResponse` carries a `NOTIFY` on a channel that the session listens to. It is delivered between transactions, never inside one, and at once to an idle session, because the notifier wakes the idle task. `ParameterStatus` is sent when a reported parameter changes, before the next `ReadyForQuery`. `NoticeResponse` carries warnings, for example before a shutdown. `LISTEN` and `NOTIFY` are part of M5, and document 11 owns the queue.

## 6.13 Settings

### 6.13.1 The parameter table

PostgreSQL 19 defines 438 parameters in `guc_parameters.dat`: 157 integer, 129 boolean, 78 string, 42 enum and 32 real. rupg vendors the file and generates a static table with the name, type, context, group, flags, default, limits, unit, enum values and descriptions of each parameter. Every one of the 438 exists in rupg. `SHOW` and `current_setting` return each one. `pg_settings` and `SHOW ALL` apply the same two filters as PostgreSQL, so a superuser sees 432 rows.

| Context | Count | Who can change it |
|---|---|---|
| `internal` | 23 | Nobody. It reports a fact, for example `server_version_num` |
| `postmaster` | 71 | Only at server start |
| `sighup` | 115 | The configuration and a reload |
| `superuser-backend` | 4 | A superuser at connection start |
| `backend` | 2 | Any user at connection start |
| `superuser` | 61 | A superuser or a role with `SET` privilege, at any time |
| `user` | 162 | Any user at any time |

A change that the context does not allow fails with the SQLSTATE and text of the pin, for example `55P02` and `parameter "%s" cannot be changed without restarting the server`.

### 6.13.2 Honored, accepted and refused

Each parameter has one of three statuses. The status is a column in the generated table, set by hand for each parameter and reviewed in the change that sets it.

1. **Honored.** rupg reads the value and behaves as PostgreSQL does.
2. **Accepted.** rupg stores and returns the value, and the value has no effect. This is right for a parameter that controls a PostgreSQL mechanism that rupg does not have, for example `random_page_cost`, `autovacuum_naptime`, `wal_buffers` or `effective_io_concurrency`. Scripts set these, and an error would break the script for no benefit.
3. **Refused.** A change from the default fails with `0A000` and `rupg does not support parameter "%s" set to "%s"`. This is right only when silent acceptance would change the meaning of data. Each entry has a reason in the table.

Some parameters map to a rupg mechanism with another shape. The mapping is part of the honored status.

| Parameter | rupg meaning |
|---|---|
| `shared_buffers` | The minimum size of the buffer pool inside `rupg.memory_limit` (document 04 section 4.6) |
| `work_mem` | The reservation of one operator before it spills |
| `maintenance_work_mem` | The reservation of one index build or mover task |
| `synchronous_commit` | `off` acknowledges a commit before its log records are durable. Every other value waits for durability. With replicas, `remote_write` and `remote_apply` follow document 18 |
| `max_parallel_workers_per_gather` | The largest number of workers that run the morsels of one query. 0 means one worker |
| `default_transaction_isolation`, `transaction_isolation` | The isolation levels of document 11. Until M5 provides `serializable`, a request for it is refused, so a client never gets a weaker level without an error |
| `session_replication_role` | Honored from M7, when triggers exist. Before M7, `replica` is refused |
| `jit` | Accepted only. rupg's compile tiers (document 14) are controlled by `rupg.compile`. asyncpg turns `jit` off to avoid PostgreSQL's compile time, and that must not turn off rupg's compiled pipelines |
| `track_activity_query_size` | Honored. It sizes the query text in each activity slot (section 6.4) |

The honored set grows with the milestones. M2 honors the reported parameters, `server_version_num`, `max_identifier_length`, `application_name`, `extra_float_digits`, `bytea_output`, `client_min_messages`, `password_encryption`, `scram_iterations` and the TLS parameters. M3 adds `search_path` with every PostgreSQL rule, the `lc_*` parameters, `plan_cache_mode`, `work_mem` and the parameters that `pg_dump` output sets. M5 adds the timeouts, the transaction parameters, `role`, `session_authorization` and `synchronous_commit`. M7 adds `plpgsql.*`, `check_function_bodies` and `session_replication_role`. The build writes a report of the three counts, and `rupg-compat` publishes it with the compatibility number.

### 6.13.3 Where values come from

The sources are those of PostgreSQL, in increasing priority: the built-in default, the server configuration, the database default, the role default, the role default for the database, the startup packet, and `SET`.

**Server configuration.** A server reads `postgresql.conf` if one is named with `config_file` or found beside the file, and the command line with `-c name=value`. `ALTER SYSTEM` writes into a shared catalog table of rupg, `rupg_auto_conf`, inside the file, in place of `postgresql.auto.conf`. A reload reads both. A file with no configuration file at all is a valid server: every setting can live inside it.

**Database and role defaults.** `ALTER DATABASE ... SET`, `ALTER ROLE ... SET` and `ALTER ROLE ... IN DATABASE ... SET` write rows of `pg_db_role_setting` inside the file, as PostgreSQL does. A session reads them at startup after authentication.

**The shared base.** The server builds one immutable set of values for each pair of database and role that has live sessions, from the sources above. Each session points to its base and holds only the values that it changed with `SET` or the startup packet. A reload or an `ALTER ... SET` makes a new base, and each session moves to it at the start of its next statement, keeping its own changes. This is how the settings part of section 6.4 stays at 2 KiB.

**Transactions.** `SET LOCAL` and a `SET` inside a transaction that rolls back are undone at the end of the transaction. Each session keeps a stack with one entry for each transaction level that changed a value. `set_config(name, value, is_local)` follows the same rules. A function with a `SET` clause pushes a level for the call.

### 6.13.4 Reporting and reset

The reported parameters are the 15 with `GUC_REPORT` at the pin: `application_name`, `client_encoding`, `DateStyle`, `default_transaction_read_only`, `in_hot_standby`, `integer_datetimes`, `IntervalStyle`, `is_superuser`, `scram_iterations`, `search_path`, `server_encoding`, `server_version`, `session_authorization`, `standard_conforming_strings` and `TimeZone`. A shim version uses its own set (document 05 section 5.6.2). The server sends a reported value at startup and again before `ReadyForQuery` when the value changed, once, with the value at that time. A custom parameter can be marked for reporting by an extension, as in PostgreSQL.

`DISCARD ALL` runs `CLOSE ALL`, `SET SESSION AUTHORIZATION DEFAULT`, `RESET ALL`, `DEALLOCATE ALL`, `UNLISTEN *`, `pg_advisory_unlock_all()`, `DISCARD PLANS`, `DISCARD TEMP` and `DISCARD SEQUENCES`, in this order, and fails in a transaction block with `25001`. `DISCARD PLANS` drops the references of the session to cached plans and keeps the prepared statements. The asyncpg reset, four statements in one `Query`, must work in one implicit transaction. A test runs each reset form after a script that creates every kind of session state and compares the result with the oracle item by item.

## 6.14 Pooling

**rupg does not need an external pooler for memory.** PostgreSQL users put PgBouncer in front of the server because each backend is a process with private caches. A rupg session costs at most 64 KiB when idle and shares its caches, so 10,000 idle connections fit in about 625 MiB by the budget of section 6.4. That is a prediction until the test of section 6.4.3 measures it.

**rupg must work behind PgBouncer.** Many deployments have a pooler that the database team does not control. PgBouncer in transaction mode depends on four things, and each one has a test with PgBouncer 1.26.0 in the client matrix.

1. The status byte of `ReadyForQuery` is correct after every message.
2. `DISCARD ALL` and `DEALLOCATE ALL` behave as in PostgreSQL. PgBouncer renames client statements to `PGBOUNCER_<n>`, keeps up to `max_prepared_statements` of them on each server connection (200 by default), and runs `DEALLOCATE ALL` to clear them. After that, a `Bind` to a dropped name fails with `26000`, and PgBouncer prepares it again.
3. `server_reset_query` and `server_check_query` run as in PostgreSQL.
4. The parameter reports reach PgBouncer, which tracks `client_encoding`, `DateStyle`, `TimeZone`, `standard_conforming_strings` and `application_name` for each client.

Because the plan cache is shared by the node, a statement that PgBouncer prepares on 50 server connections has one plan, not 50.

**No built-in pooler at first.** The design has no pooler inside the server. The reason for one would be a client population larger than `max_connections` that cannot use a pooler. Document 24 holds this as open question 6.A, to be decided by the measurement of section 6.4.3.

## 6.15 Limits, sockets and shutdown

**Limits.** The limits are those of PostgreSQL, so a client sees the same failure.

| Limit | Value |
|---|---|
| Startup packet | 10,000 bytes |
| Small messages before authentication | 10,000 bytes |
| Any message | 1 GB minus 2 |
| Parameters for one statement | 65,535 |
| Columns in one result | 1,664 |
| Identifier length | 63 bytes |

A protocol violation is `08P01` at the level of PostgreSQL. A bad length closes the connection without a message. An unknown type byte gets one `FATAL` and a close. A body that is wrong inside a complete frame is an `ERROR`, and the session goes on.

**Connection limits.** `max_connections` defaults to 100, `superuser_reserved_connections` to 3 and `reserved_connections` to 0, as in PostgreSQL. When the server is full, a new session gets `FATAL` `53300` with `sorry, too many clients already`. A user may raise `max_connections` far higher than with PostgreSQL. The default stays at 100 so that a pooler or an ORM sees the behavior it expects.

**The Unix socket.** The server listens on `<dir>/.s.PGSQL.<port>` for each directory in `unix_socket_directories`, and creates the lock file `<dir>/.s.PGSQL.<port>.lock` with the PostgreSQL contents, so that a second server on the same port fails to start. The default port is 5432. The default directory is `/tmp`, which is the default of a PostgreSQL source build. Packages can change it. This socket is the PostgreSQL protocol socket. The socket that a second process uses to reach the owner of a file is another socket, and document 17 owns it.

**Shutdown.** The server supports the three modes of PostgreSQL with the signals that `pg_ctl` sends.

| Mode | Signal | Behavior |
|---|---|---|
| Smart | `SIGTERM` | Refuse new sessions. Wait for all sessions to end. Checkpoint and exit |
| Fast | `SIGINT` | Refuse new sessions. Send each session `FATAL` `57P01` `terminating connection due to administrator command`. Roll back open transactions. Checkpoint and exit |
| Immediate | `SIGQUIT` | Send each session `WARNING` `57P01` `terminating connection due to immediate shutdown command` and close it. Exit without a checkpoint. The next open recovers from the log rings (document 11) |

A stronger mode takes over a weaker one in progress. A new connection during shutdown gets `FATAL` `57P03` with `the database system is shutting down`. The server writes the PostgreSQL log lines for each mode. The checkpoint is the one of document 08 section 8.6. The library has the same three modes as calls on the server handle.

## 6.16 Replication connections and FunctionCall

**Logical replication.** A startup packet with `replication=database` starts a replication session, which accepts the replication command language and uses `CopyBothResponse`. rupg implements logical replication out and in at M9: `CREATE PUBLICATION`, `CREATE SUBSCRIPTION`, replication slots, the `pgoutput` plugin and the commands `IDENTIFY_SYSTEM`, `CREATE_REPLICATION_SLOT`, `START_REPLICATION` and `READ_REPLICATION_SLOT`, with the 19 messages `PrimaryStatusRequest` and `PrimaryStatusUpdate`. Debezium and PeerDB need this. The LSN that rupg reports is the commit timestamp of document 11 section 11.12. Before M9, a replication session fails at startup with `0A000`.

**Physical replication.** A startup packet with `replication=true` asks for physical streaming. rupg never supports it, because its replicas use Raft (document 18). The error is `0A000` at startup. Document 05 section 5.3 excludes it.

**FunctionCall.** The legacy fast-path message calls a function by OID. pgjdbc and libpq use it for large objects. rupg implements it at M9 with large objects. Before M9, it fails with `0A000`.

## 6.17 Observability

**pg_stat_activity.** The view has the 22 columns of the pin. The session table fills them. `state` uses the PostgreSQL strings: `active`, `idle`, `idle in transaction`, `idle in transaction (aborted)` and `fastpath function call`. `backend_type` is `client backend` for a session and the PostgreSQL names for the background tasks that have a PostgreSQL counterpart, for example `checkpointer`. `wait_event_type` and `wait_event` show the PostgreSQL names where the wait has the same meaning, for example `Lock` and `transactionid` while a session waits for a row lock, and `Client` and `ClientRead` while it waits for input. `backend_xid` and `backend_xmin` show the 32-bit transaction ids of document 11 section 11.9. pgAdmin, DBeaver and most monitoring agents read this view.

**The log.** The server writes its log to standard error in the PostgreSQL text format. It honors `log_line_prefix` for the common escapes (`%m`, `%p`, `%u`, `%d`, `%a`, `%h`), `log_min_messages`, `log_statement`, `log_min_duration_statement` and `log_connections`. pgBadger parses this format. Document 05 section 5.3 excludes log lines from the comparison, but the lines that log analyzers parse keep the PostgreSQL text.

## 6.18 Performance targets

The protocol puts a floor under every statement. On the machine `gpc` (i9-13900K, WSL2), `SELECT 1` against PostgreSQL takes about 13 µs over a Unix socket and about 30 µs over TCP on loopback (`../2140/compat/postgres/15-performance.md` section 15.2). Most of this is the client library, the system calls on both sides and the context switches. No server design removes it. So rupg does not claim 10x latency for one point statement at one client. Document 02 section 2.7 and document 20 state the transactional claims.

| Measure | Target | Milestone |
|---|---|---|
| Idle session memory | At most 64 KiB, section 6.4 | M2 |
| Connection rate with `trust` over a Unix socket | At least 10x the oracle | M2 |
| Connection rate with SCRAM over TLS | At least 10x the oracle, with the same iteration count | M2 |
| Wire overhead on analytic suites | The sum over a suite through the server is within 1 percent of the same sum in process | M4 |
| `COPY FROM STDIN` load time for ClickBench `hits` and TPC-H at scale factor 10 | At least 10x faster than the oracle with its fastest legal configuration | M4 |
| Pipelined point operations at depth 64 | Server CPU per operation 10x below the oracle | M8 |

The connection rate target is a prediction based on the cost of a process fork in PostgreSQL against the cost of a task in rupg. SCRAM with 4,096 iterations of PBKDF2 costs the same CPU on both servers, so the TLS and SCRAM target depends on the rest of the startup being small. If the target fails because of PBKDF2 alone, we publish the number with that cause and do not lower the iteration count. The 1 percent rule comes from `../2140/compat/postgres/15-performance.md`. Document 20 runs these measurements.

## 6.19 Gates

| Milestone | What this document must deliver |
|---|---|
| M2, gate G1 | Startup, 3.0 and 3.2, negotiation and grease, `SSLRequest` and direct TLS, SCRAM with `-PLUS`, MD5, password, peer and cert, host rules inside the file, the simple flow, cancel, the 15 reported parameters, `SHOW` of all 438 parameters, shutdown. psql, libpq and the 14 drivers of the connect matrix connect with byte-identical traces. The idle session gate of section 6.4 and the connection rate targets |
| M3, gate G4 | The extended flow, prepared statements, portals, pipelining with the 9 `libpq_pipeline` traces, the shared plan cache, `RevalidateCachedQuery`, `DISCARD` in every form, PgBouncer in transaction mode |
| M4 | `COPY` in every format, with the load target |
| M5, gate G5 | Timeouts, `LISTEN` and `NOTIFY` delivery, the transaction parameters |
| M9 | Logical replication out and in, `FunctionCall`, SNI, `oauth`, Windows, the shim protocol rules for 14 to 18 |
| M12, gate G12 | `ldap`, `pam`, and L1 and L5 at 100 percent for every client |

## 6.20 Open questions from this document

6.A. Should the server have a built-in pooler for client populations above `max_connections`? Section 6.14 says no until the idle session measurement exists.

6.B. Is the TLS state after the handshake within 24 KiB? If it is not, should rupg use kTLS on Linux to move the record buffers into the kernel, and how does that interact with `io_uring` receives and `-PLUS` channel binding?

6.C. Can a session move between workers often enough to balance load without breaking the per-ring receive buffers? Section 6.2 allows a move only at a statement boundary. A measurement under a mix of TPC-C and ClickBench sessions decides whether that is enough.

6.D. Should `jit` stay accepted only? A user who sets `jit = off` to remove compile latency may expect rupg to stop compiling too. Section 6.13.2 keeps rupg's tiers on.
