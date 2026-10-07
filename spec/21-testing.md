# 21. Testing

Written 7 October 2026. Pins: PostgreSQL `REL_19_STABLE` at `7d3d2db7` (19beta4), and the minor releases 18.6, 17.11, 16.15, 15.19 and 14.24. The suite counts in this document come from the source tree at the pin. The client count comes from `../2140/compat/postgres/13-clients-and-tools.md`. The fuzzing hours, the crash point counts, the simulation hours, the thresholds and the CI times are budgets. They are not measurements, and the milestone reports of document 23 replace them with measured numbers.

This document says how rupg shows that it is correct. A database that is 10x faster and gives a wrong answer has no value, so every claim in documents 02 and 20 depends on these tests. The oracle is a real PostgreSQL server. The tests compare rupg with the oracle, with itself in a different layout, with its own text forms, and with a model of the disk, the clock and the network.

## 21.1 The rules

**A wrong answer is the worst bug.** The order of severity is: a wrong answer, a lost or partial commit, a corrupt file, a crash, a panic, an error that PostgreSQL does not raise, a missing feature, and a slow query. A milestone must not close with a known bug of the first three classes.

**Any panic is a failure.** This is true also in a test that expects an error. An error in rupg is a value with a SQLSTATE (document 04 section 4.8), and a panic is a defect.

**Every bug gets a regression test before its fix.** The test fails before the fix and passes after it, at the lowest layer that can show the bug.

**The oracle decides.** When the PostgreSQL documentation and the oracle disagree, the oracle is correct. When the oracle gives an answer that depends on the run, for example the order of rows without `ORDER BY`, the harness runs the oracle twice and removes the unstable case from the comparison. Document 05 lists the bytes that the comparison excludes. A test must not add an exclusion. Only a change to document 05 can add one.

**A test runs on one layer when it can.** Every layer has a text form (document 04 section 4.8). A test for the optimizer starts from a query tree in text and does not parse SQL. A test for the cold store starts from a segment in text and does not plan a query.

**A randomized test prints its seed.** Every generator, fuzzer and simulation prints the seed at the start and the end. A failing seed reproduces the failure on any machine of the same tier.

**A number may not go down.** The pass counts, the fuzzing hours without a find, and the performance numbers have a ratchet (section 21.15). A change that lowers a ratchet number fails CI. This is the ratchet rule of `../2140/compat/postgres/17-the-order-of-work.md`.

## 21.2 The test layers

"Per change" means on each pull request and on each commit to `main`. The time budgets are wall time on the CI machines of section 21.16.

| Layer | What it checks | When | Time budget |
|---|---|---|---|
| T1 Unit and property | one function or one structure in one crate | per change | 6 minutes |
| T2 Text form | the text form of each layer, both directions | per change | 2 minutes |
| T3 Differential | rupg and the oracle on the same input, byte for byte | per change (a subset), nightly (all) | 15 minutes, 3 hours |
| T4 PostgreSQL suites | regression, isolation, TAP, `libpq_pipeline`, contrib | nightly | 2 hours |
| T5 Client suites | the test suites of 48 clients | nightly, by group | 4 hours |
| T6 Generated queries | TLP, NoREC, PQS, layout equivalence | nightly | 3 hours |
| T7 Fuzzing | parsers and decoders on hostile input | continuous | section 21.7 |
| T8 Crash | every write and sync order that the disk can give | per change (a subset), nightly | 10 minutes, 4 hours |
| T9 Simulation | the whole engine under a seeded scheduler | nightly | section 21.10 |
| T10 Concurrency | latches, rings and queues under every interleaving | per change (loom), nightly | 5 minutes, 2 hours |
| T11 Jepsen and Elle | histories of real clients against real nodes | weekly from M5 | 8 hours |
| T12 Resources | memory, CPU and disk per operation and per session | nightly | 1 hour |
| T13 Performance | instructions and time on fixed queries | per change (instructions), nightly (time) | 20 minutes |

T1, T2, T8 and T10 need no PostgreSQL, and a contributor runs them with `cargo test`. T3 to T6 and T11 need the oracle and run in `rupg-compat`. T13 runs in `rupg-bench`.

## 21.3 The harness

### 21.3.1 Why a separate repository

The harness is in `github.com/tamnd/rupg-compat`, a sibling of `rupg` (document 22 section 22.12). It talks to rupg only through a socket, the C API and the libpq exports. It does not link a rupg crate. This is the rule of `../2140/compat/postgres/14-rudb-postgres.md`. If the harness used the engine's codec, a bug in the codec would be on both sides of the comparison and the comparison would not find it.

### 21.3.2 The layout

The layout follows `rudb-postgres` in `../2140/compat/postgres/14-rudb-postgres.md`, with additions for the TAP subset and for Jepsen.

```
rupg-compat/
  pins.toml          PostgreSQL commits, client versions, rupg commit
  oracle/            build scripts and the fixed configuration
  src/               servers, frame, proxy, trace, replay, compare, exclude,
                     differential, generate, reduce, bisect, regress,
                     isolation, tap, clients, report
  clients/<name>/    run.sh, oracle-fail.txt, expected-fail.txt
  corpus/            traces, generated, reduced and fuzz cases
  jepsen/            the Jepsen test for rupg
  ratchet.toml       the best number of each report row
  reports/<date>/    one directory per published run
```

### 21.3.3 The oracles

The harness builds one oracle from source for each version that rupg claims, with the same configure options and the same configuration.

| Oracle | Source | Purpose |
|---|---|---|
| 19 | `REL_19_STABLE` at `7d3d2db7`, then the `REL_19_0` tag after 29 October 2026 | the default `rupg.compat_version` |
| 18 | `REL_18_6` | the 18 shim |
| 17 | `REL_17_11` | the 17 shim |
| 16 | `REL_16_15` | the 16 shim |
| 15 | `REL_15_19` | the 15 shim |
| 14 | `REL_14_24` | the 14 shim, kept after the end of life on 12 November 2026 while document 05 lists it |

A change to a pin is one commit to `pins.toml`, and each change is a run of every suite. The 19 pin moves to RC1 on 15 October 2026 and to 19.0 on 29 October 2026.

The fixed configuration is the configuration of `../2140/compat/postgres/14-rudb-postgres.md`. The same file configures the rupg server under test.

| Setting | Value | Why |
|---|---|---|
| `initdb` locale | `--no-locale`, encoding `UTF8` | the C collation gives one sort order on every machine |
| `TimeZone` | `UTC` | no daylight saving changes in expected output |
| `DateStyle` | `ISO, MDY` | the regression suite default |
| `lc_messages` | `C` | messages in English, equal on both sides |
| `password_encryption` | `scram-sha-256` | the default of the clients |
| `ssl` | `on`, with a fixed test certificate | the clients test TLS |

A second nightly set uses an ICU collation and a time zone that is not UTC. Its failures are bugs, but it is not the gate set.

### 21.3.4 The command line

The commands are those of `../2140/compat/postgres/14-rudb-postgres.md`, plus `tap` and `jepsen`. `up`, `client`, `regress`, `isolation`, `tap` and `jepsen` start the servers or run one suite. The others are:

| Command | What it does |
|---|---|
| `rupg-compat oracle build [--version V]` | builds the oracle of version V from its pin |
| `rupg-compat record <client>` | runs a client through the proxy and writes a trace |
| `rupg-compat replay <trace>` | sends a trace to both servers and compares the answers |
| `rupg-compat diff <sql>` | runs one statement on both servers and shows the first different byte |
| `rupg-compat gen --oracle tlp\|norec\|pqs\|diff --seed S` | runs the generator of section 21.4.5 |
| `rupg-compat reduce <case>` | makes a failing case smaller while it still fails |
| `rupg-compat bisect <case>` | finds the first rupg commit where a case fails |
| `rupg-compat report` | writes `reports/<date>/` and checks the ratchet |

Every command takes `--compat-version V`, which picks the oracle of version V and sets `rupg.compat_version` to V on rupg.

### 21.3.5 Record and replay

The proxy sits between a client and a server and writes each message in both directions to a trace. The trace is text, one message per line, so a reviewer can read it and a test can be written by hand. Replay has three problems, and the harness solves each one as `rudb-postgres` does.

1. **OIDs differ.** A table that the oracle made has a different OID on rupg if the order of creation differs. Replay keeps a map from the oracle's OIDs to rupg's OIDs and rewrites the messages. Built-in objects have PostgreSQL's OIDs (document 00), so the map holds only user objects.
2. **Authentication differs.** A SCRAM exchange has random nonces. Replay authenticates again with each server.
3. **Cancel keys differ.** Replay rewrites each `CancelRequest` to the key that the receiving server sent, at the key length of protocol 3.0 or 3.2.

### 21.3.6 The comparison

The comparison is byte for byte on every message that a client can see, after the OID map. Document 05 lists the excluded bytes, for example the process identifier and the secret key in `BackendKeyData`, the server's nonce in SCRAM, and some parameter values in `ParameterStatus`. The report prints each exclusion that a run used, with its count.

An error is equal when the SQLSTATE, the severity, the message, the detail, the hint and the position are equal. The file, line and routine fields are excluded, because they name PostgreSQL's C source. Document 05 decides this.

When a statement has no `ORDER BY` that fixes the order, the harness sorts both row sets before it compares them.

### 21.3.7 Reduce and bisect

The reducer makes a failing case smaller in three passes: it removes statements with delta debugging, then clauses and expressions, then makes literals smaller. It keeps a step only if the case still fails in the same way, which means the same first different message type and the same SQLSTATE. This stops a reduction from moving to a different bug.

`bisect` builds rupg between a good commit and a bad commit, so a bisect over 1,000 commits builds about 10.

## 21.4 The suites

### 21.4.1 The PostgreSQL suites

| Suite | Size at the pin | Level | How rupg runs it | First milestone | Gate milestone |
|---|---|---|---|---|---|
| Regression | 239 tests, 290,977 lines of expected output | L3 | `rupg-compat regress`, the parallel schedule, against a rupg server | M3 (a tracked pass count) | M7 (the full schedule, G6 milestone), M12 (100 percent, G12) |
| Isolation | 135 specs | L4 | `rupg-compat isolation`, every permutation in each spec | M5 | M5 (G5 milestone), M12 |
| `libpq_pipeline` | 9 traced tests | L1 | the test program from the pin, linked to the oracle's libpq, against rupg | M2 | M2 |
| TAP | 294 files | L4 and L5 | the subset of section 21.4.1 | M9 | M12 |
| Contrib regression | 61 directories, 55 extensions | L3 | `rupg-compat regress` with the contrib schedules | M7 | M12 |

Document 05 section 5.6 gives the suites for each shim version. For each shim version V, the harness runs the suites of version V against the oracle of version V and against rupg with `rupg.compat_version` set to V. A shim passes when the pass sets are equal.

**The TAP subset.** Most TAP files test PostgreSQL's own programs and directories: `pg_ctl`, `pg_basebackup`, `pg_upgrade`, `initdb`, the WAL format and the data directory. rupg has no data directory and no WAL in PostgreSQL's format, so those tests do not apply as they are written. The TAP subset is the set of files whose checks a client can see over the protocol: authentication, `pg_hba.conf` rules, SSL, `psql` behavior, `pg_dump` and `pg_restore` output, logical replication output, and the recovery checks that test a result and not a file. Document 05 owns the list. The count of files in the subset is an open question for document 24.

`pg_dump` from each oracle version must dump a rupg database, and a restore of that dump into the oracle must dump to the same text. This test of L2 and L5 runs nightly from M2.

### 21.4.2 The client suites

The 48 clients of `../2140/compat/postgres/13-clients-and-tools.md` each have a directory in `clients/`. `run.sh` installs the client at its pinned version and runs its test suite against a server. Two files go with it.

1. `oracle-fail.txt` lists the tests that fail against the oracle. They are not counted.
2. `expected-fail.txt` lists the tests that pass against the oracle and fail against rupg. Each line has the cause and the milestone that will fix it. The file may only become shorter. A test that starts to pass is removed from it in the same change.

Gate G1 at M2 needs 7 clients to connect and show the schema. The seven are psql, pgAdmin 4, DBeaver (through pgjdbc), psycopg 3, node-postgres, pgx and asyncpg. "Show the schema" means the client's own object browser or describe command lists the tables, the columns, the types, the indexes and the constraints of the gate schema, and the list is equal to the list that the same client shows against the oracle. Document 02 does not name the seven, so this choice is an open question for document 24.

### 21.4.3 Hermitage

Hermitage, by Martin Kleppmann, shows which anomalies each isolation level allows. rupg runs its PostgreSQL tests at `READ COMMITTED`, `REPEATABLE READ` and `SERIALIZABLE`. The result for each anomaly must be the result that the oracle gives. A level that is stronger than PostgreSQL's is also a failure, because an application can depend on a statement that does not block.

### 21.4.4 sqllogictest

rupg runs the sqllogictest corpus of SQLite and compares its answers with the oracle's answers, not with the expected answers in the corpus. Those come from engines with other rules for integer division and for the type of an average. The value of the corpus is its size.

### 21.4.5 Generated queries

The generator builds random schemas, random data and random queries. It uses the productions of the vendored `gram.y` (3,476 productions at the pin) as its grammar, with a weight on each production. The weight of a production goes up when no generated statement in the last run used it, so the generator moves toward the parts of the grammar that the tests have not reached. Each generated query is checked by one of five oracles.

| Oracle | Source | What it checks |
|---|---|---|
| Differential | `../2140/compat/postgres/14-rudb-postgres.md` | rupg and PostgreSQL give the same answer |
| TLP | Rigger and Su, OOPSLA 2020 | a query equals the union of its three partitions on a predicate `p`: `p`, `NOT p` and `p IS NULL` |
| NoREC | Rigger and Su, ESEC/FSE 2020 | a filter in `WHERE` gives the same count as the same predicate in the select list, which an optimizer does not touch |
| PQS | Rigger and Su, OSDI 2020 | a row that the generator picked is in the result of a query whose predicate is true for that row |
| Layout equivalence | this document | the same query on the same data in different layouts gives the same answer |

TLP, NoREC and PQS need no oracle server, so they also run inside the simulation of section 21.10.

A rupg table can have rows in the hot store, in the cold store, or in both (document 10). The harness loads the same data five ways and runs each query on each.

1. Hot only. The mover is off.
2. Cold only. The data is loaded with `COPY` and the mover has moved every row.
3. Mixed. Half the rows of each table have moved, chosen at random by the seed.
4. Mixed after change. The rows were moved and then updated and deleted, so the cold store has deletes and the hot store has new versions.
5. No summaries. The same as case 3, with every summary deleted (section 21.8).

The same oracle compares one worker with many workers and each compile tier of document 14. A mode must not change an answer or an error.

### 21.4.6 Benchmark consistency

A benchmark number is published only when the answers of the run are correct. Each benchmark has a check.

| Benchmark | Check |
|---|---|
| TPC-C | the consistency conditions 1 to 4 of the TPC-C specification, clause 3.3.2, after every run |
| pgbench | the sum of `abalance` equals the sum of `tbalance` and the sum of `bbalance`, and the history row count equals the transaction count |
| TPC-H | the answers at SF1 equal the qualification answers of the specification, and the answers at SF100 equal the oracle's answers |
| ClickBench | the result of each of the 43 queries equals the oracle's result on the same data |
| YCSB | each read returns the value of the last acknowledged write to that key in a run with one client per key |

A run whose check fails is not reported as a number. It is reported as a wrong-answer bug.

## 21.5 Unit and property tests

Each crate has unit tests and `proptest` property tests, with a fixed case count per change and a larger count at night.

**The kernels have one answer for each path.** `rupg-kernels` has an AVX-512 path, an AVX2 path, a NEON path, a portable path and a 128-bit WebAssembly path (document 04 section 4.10). For each kernel, a property test runs every path that the machine has on the same input and compares the output bits. The inputs include the lengths 0, 1, 1023, 1024 and 1025, each start alignment from 0 to 63 bytes, all nulls, no nulls, and the limits of each type. A floating point kernel must use the same order of operations on each path, so a sum does not depend on the instruction set.

**Each tier gives the same result and the same error.** The interpreter, the vectorized executor and the compiled tier of document 14 evaluate the same expression on the same input. The result and the SQLSTATE must be equal, also for overflow, division by zero and an invalid cast.

**Each encoding is lossless.** For each encoding of document 09, a property test encodes a generated column and decodes it. The output must equal the input bit for bit, also for `NaN` payloads and negative zero. A test also runs the chooser on columns at the boundary between two encodings.

**Each type has a stable text and binary form.** For each of the 197 types, generated values go through input, output, send and receive. The text must be stable after one round trip, and both forms must equal the oracle's forms.

**The point path and the vectorized path agree.** A query that the point path can run is also run by the vectorized executor with the point path off. The answers must be equal.

## 21.6 Text form round trips

Each layer prints its main structure as text and parses the text back (document 04 section 4.8). For a generated structure `x`, `parse(print(x))` must equal `x`. For each hand-written file `s`, `print(parse(s))` must equal `s`, which keeps the format readable.

| Structure | Crate | Text form |
|---|---|---|
| Parse tree | `rupg-sql` | SQL, printed with every parenthesis that the tree needs |
| Query tree | `rupg-analyze` | the tree with OIDs and types |
| Physical plan | `rupg-plan` | the plan, a superset of `EXPLAIN (VERBOSE)` (document 15 section 15.9) |
| Segment metadata | `rupg-column` | the encodings, the summaries and the offsets of a cold segment |
| Page | `rupg-hot`, `rupg-tree` | the header, the slots and the tuples or keys |
| Log record | `rupg-log` | one line per record |
| Compiled pipeline | `rupg-jit` | the IR text, lifted from the printer and parser of `rudb-qc-ir` |
| Catalog | `rupg-catalog` | the objects of a schema as SQL |

`cargo xtask bless` rewrites the expected files in `crates/<crate>/tests/text/`, and the reviewer reads the difference.

## 21.7 Fuzzing

Fuzzing runs with `cargo-fuzz` and libFuzzer in the separate `fuzz/` workspace on the nightly toolchain, as in rudb (document 22 section 22.2). Each target has a corpus in `rupg-compat/corpus/fuzz/<target>/`.

Every target fails on a panic, on a hang over 1 second, and on memory over its budget. The table gives the other failure of each target.

| Target | Input | Crate | Also a failure |
|---|---|---|---|
| `wire_frontend` | client bytes after the startup | `rupg-wire`, `rupg-session` | a reply that the protocol state does not allow |
| `wire_startup` | startup, SSL and GSS requests, SCRAM | `rupg-wire`, `rupg-server` | a session that starts without the correct password |
| `sql_parse` | SQL text | `rupg-sql` | an accept or reject that differs from PostgreSQL's parser |
| `type_input`, `type_recv` | a value and a type OID | `rupg-types` | an accept, reject or output that differs from the oracle |
| `file_open` | a `.rupg` file with changed bytes | `rupg-file`, `rupg-log` | data that is not in the file, with no `XX001` error |
| `page_decode` | a hot page or a tree page | `rupg-hot`, `rupg-tree` | a read outside the page |
| `segment_decode` | a cold segment | `rupg-column`, `rupg-encode` | a value that the segment does not hold |
| `log_replay` | a log ring | `rupg-log` | a state that is not a prefix of the committed state |
| `plpgsql_parse` | a function body | `rupg-plpgsql` | an accept or reject that differs from the oracle |
| `jit_verify` | a pipeline in IR text | `rupg-jit` | compiled code that differs from the interpreter |
| `capi_calls` | a sequence of C API calls | `rupg-capi` | a leak or a use after free under ASan |
| `import_dump` | a `pg_dump` archive | `rupg-import` | a database that differs from the oracle's restore |

The `sql_parse` target compares with libpg_query, which is PostgreSQL's parser built as a library under the BSD license. Only `rupg-compat` uses it.

The budget is in CPU hours for each target. A milestone closes only when each of its targets has run its hours with no open find. The compat PG1 gate of rudb had 24 hours of fuzzing (`../2140/compat/postgres/17-the-order-of-work.md`), and rupg starts from that number.

| Milestone | Targets that join | CPU hours per target before the milestone closes |
|---|---|---|
| M1 | `file_open`, `page_decode`, `log_replay` | 24 |
| M2 | `wire_frontend`, `wire_startup`, `type_input`, `type_recv` | 24 |
| M3 | `sql_parse` | 48 |
| M4 | `segment_decode` | 48 |
| M6 | `jit_verify` | 48 |
| M7 | `plpgsql_parse` | 48 |
| M9 | `capi_calls`, `import_dump` | 48 |
| M12 and 1.0 | every target | 30 days on each target with no new find |

The 1.0 rule comes from `../2140/16-testing.md`: no known wrong-answer bug and a month of fuzzing without a new find.

**At M1.** The three M1 targets are in `fuzz/`. They start from one base file on the simulated disk with 32 commits before a checkpoint and 16 after it, and no close. `file_open` changes any byte of the file and can cut it, and an open must give the rows after one of the commits. `page_decode` changes one hot page after its first 24 bytes and seals the page again, so the decoders of the hot and tree pages see the changed bytes. `log_replay` changes the log after the redo position, and an open must give the rows after a commit at or after the checkpoint, or `XX001`. Each target runs `rupg check` before the open. The corpus is not yet in `rupg-compat`. The first runs found two bugs. A file of zero bytes opened as a new database when `create` was off, and now gives `58P01`. A child number in an inner page that pointed up the tree made the descent loop, and now each descent checks that the level goes down and gives `XX001`.

## 21.8 Delete every summary and rerun every suite

This is the rule that keeps the summaries of H1 to H6 honest (document 02 section 2.4.5). A summary is a copy of a fact about the data. It must never be the only source of a fact. So rupg must give the same answers when every summary is gone.

The test does this:

1. Take a copy of the file at the end of a suite's setup.
2. Run `rupg strip --summaries` on the copy, with the `rupg` binary of `rupg-cli`. It deletes every structure that document 02 calls a summary: the segment summaries of document 09 section 9.7, the zone maps, the frequency synopses of document 12 section 12.8, the gram summaries of document 12 section 12.9, the per-distinct result tables of document 14 section 14.6, and the sorted projections of document 10 section 10.7. It keeps the data, the dictionaries of document 09 section 9.5 and the catalog, and it writes a file that is valid.
3. Run every suite again on the stripped copy: the regression suite, the isolation suite, the generated corpus, the ClickBench and TPC-H answers and the client suites.
4. Compare the answers with the answers on the original file. They must match byte for byte, with the same order where the query fixes the order.

The dictionaries stay, because they are the data of a coded column and not a summary of it. The indexes stay, because an index is part of the schema and `pg_index` shows it. A second run drops every index that is not a constraint and compares again, and that run is also nightly.

`rupg.use_summaries = off` is a test setting that makes the planner and the executor ignore the summaries without a change to the file. The per-change subset uses the setting, because it is fast. The nightly run uses `strip`, because the setting tests only the code that reads the setting, and `strip` tests that no other code reads a summary.

A summary must also stay correct after a change to the data. The layout equivalence oracle (section 21.4.5) updates and deletes rows after the move, and then compares the answer with summaries and the answer without them.

## 21.9 Crash and power cut

### 21.9.1 The model of the disk

`rupg-platform` has an I/O trait, and every read, write and sync in the engine goes through it. The test implementation records each operation. When the test stops the engine at a point, it builds the file that a power cut at that point could leave. The model is the model of `../2140/16-testing.md`, with these rules.

1. A write before the last completed `fdatasync` of the file is on the disk.
2. A write after the last completed `fdatasync` may be on the disk, may be lost, or may be torn. A torn write has a prefix of its 4 KiB blocks on the disk. The model also tries 512 byte sectors, for the disks that tear at that size.
3. Writes after the last sync may reach the disk in any order.
4. A failed `fdatasync` is fatal. After `EIO` from a sync, the engine must stop and recover from the log on restart. It must not retry the sync and then continue, because the kernel may have dropped the dirty pages. PostgreSQL learned this in 2018 and now stops on a sync failure by default.

The header slots, the log rings and the out-of-place writes of document 08 section 8.6 each have a rule about what is on the disk before a commit is acknowledged. The crash test checks those rules from the outside.

### 21.9.2 Enumeration

For each sync point, the test lists the unsynced writes and builds a file for subsets of them, with tears. For a small set it builds every subset. For a larger set it builds every prefix in issue order and in reverse order, and a seeded sample. The sample always has cases where one log ring has its last write and another ring does not.

After it builds a file, the test opens it, runs recovery, and checks four things.

1. Every transaction that the engine acknowledged before the cut is present, with all its changes.
2. No transaction that the engine did not acknowledge is partly present. It is either all present or all absent.
3. `rupg check` reports no error. It reads every page, every segment and every index and checks each checksum, each reference and each index entry against the table.
4. The answers to a fixed set of queries equal the answers of a model that holds only the acknowledged transactions.

**At M1.** `rupg check FILE` opens the file to read and does not replay the log, so it checks the pages of the last checkpoint and the log after it. It checks the header slots, the page table and the free space map. It reads the catalog page and, for each table, each page of the tree and each row. A row must decode against the columns of its table, have no owner, have a commit timestamp at or below the checkpoint and have a row id that the table gave. Each logical page in use must be the catalog page or a page of exactly one table. Each block of the log after the redo position must read again with a good checksum and name only tables of the catalog. M1 has no segments and no indexes. The crash test of the facade runs the check after each power cut, before the next open.

### 21.9.3 Real kernels

The model can be wrong about a real file system, so two nightly tests use real kernels on Linux.

1. **dm-log-writes.** The device mapper target records each write and flush. The test runs a workload on ext4 and XFS, replays the log to each flush mark and to points between marks, and runs the checks of section 21.9.2.
2. **LazyFS.** LazyFS is a FUSE file system that can drop unsynced data on command. The test drops the data at a random point and restarts.

A third test kills the process with `SIGKILL` at random times during TPC-C and checks the consistency conditions after each restart. It tests recovery at a high rate.

**At M1.** The scripts are in `tests/realdisk`, and the `realdisk` job of the nightly workflow runs them. They run the seeded workload of the T8 test with the `workload` mode of `rupg-crash`, which prints a digest of the tables and rows after each acknowledged change. `logwrites.py` makes ext4 or XFS on a loop device, puts dm-log-writes over it, and marks the log after each acknowledged change. It replays the log on a copy of the image after `mkfs`, and at each check point it mounts a dm-snapshot of the copy and runs the `verify` mode. The check points are a seeded sample of the flush, FUA and mark entries and of the entries between them. `kill.py` kills the workload at a random point in each round, and with `--lazyfs` it runs on LazyFS 0.3.1 and drops the data that was not synced after each kill. After each crash the tables must hold the last acknowledged change, or that change and the next one. At M1 there is no SQL, so the `SIGKILL` test uses the key-value workload in place of TPC-C. A build of rupg with no `fdatasync` failed the dm-log-writes test and the LazyFS test.

### 21.9.4 Counts

The budget is 1 million distinct crash files at M1, for key-value writes through the Rust API, header slot changes and checkpoints. Each of M3 (DML, DDL and the catalog), M4 (the mover, `COPY` and segment writes), M5 (two-phase commit, savepoints and `NOTIFY`), M6 (each index access method) and M10 (Raft log and snapshot writes) adds 1 million more for its new workloads.

## 21.10 Deterministic simulation

rupg runs the whole engine in one thread under a seeded scheduler, as FoundationDB and the VOPR of TigerBeetle do. The same seed gives the same run, bit for bit.

`rupg-platform` defines five traits: the clock, the file I/O, the network, the task scheduler and the source of random numbers. The engine reaches the outside world only through them. A rule in `cargo xtask style` rejects a call to `std::time`, `std::thread`, `std::fs`, `std::net` or the operating system's random source outside `rupg-platform`, with the `disallowed-methods` and `disallowed-types` settings of clippy (document 22 section 22.3).

The simulator runs a server, many clients and, from M10, many nodes in one process. It injects these faults, each with a rate that the seed picks:

1. I/O errors, slow I/O and torn writes on a power cut, with the model of section 21.9.1;
2. a crash and restart of one node;
3. network delays, drops, duplicates and partitions between nodes, and between clients and nodes;
4. clock skew and clock jumps, which test the HLC of document 04 and its 48 bits of milliseconds;
5. memory limits that fail an allocation (section 21.11).

The clients record each request and reply. At the end of a run, the simulator checks the history with Elle (section 21.12) and the invariants of each workload. A failing seed goes into `tests/sim/seeds.txt` with its cause.

The budget is 100 CPU hours of simulation each night from M5, and 500 CPU hours each night from M10, when the cluster joins. 

## 21.11 Concurrency tests

**loom.** The latches of the buffer manager (document 08 section 8.8), the reservation of space in a log ring, the lock manager queues and the session task queues each have a loom test. They run per change.

**At M1.** The loom tests build with `--cfg loom`, and the `loom` job of CI runs `cargo test --release -p rupg-buffer -p rupg-log --lib loom` on each change. The pool keeps its state words in the memory of the window, so the latch tests include `latch.rs` a second time with the atomics of loom. They check that an exclusive latch excludes the other latches, that shared latches count their readers, and that an optimistic read that passes `still` saw the page of one version. The ring test runs 2 or 3 threads that each place a block and flush to its end. Each flush must leave every block before the durable position in the store. The ring takes the lock and the condition variable of loom, and a loom atomic marks each write, sync and read, because loom reorders only the steps that use one loom object. With 3 threads the test bounds the preemptions at 2, because the full search takes minutes. A test does not wait for a latch in a loop, because under loom a load in a loop can see the old value with no end. Each test failed when the code had a bug on purpose: a missing fence in `try_exclusive` or `still`, a shared latch on an exclusive frame, or a flush that did not mark itself as running.

**Miri.** Miri runs the tests of the crates that may contain `unsafe` (document 22 section 22.1) each night, on the scalar and portable paths. The SIMD paths are checked by the equivalence tests of section 21.5.

**ThreadSanitizer and AddressSanitizer.** The full T1 and T8 sets run each night under TSan and under ASan on Linux x86-64, with the nightly toolchain.

**Allocation failure.** `rupg-platform` accounts every allocation that a query makes against `rupg.memory_limit`. A test mode fails the Nth allocation of a query, for each N. The query must fail with SQLSTATE `53200`, and the session must stay usable with no leak, as in `../2140/16-testing.md`.

**Spill.** Each operator that can spill (sort, hash join, hash aggregate, window, the exchange buffers) has a test that sets the memory limit so low that the operator spills at each level it has. The answers must equal the answers without a spill.

## 21.12 Jepsen and Elle

Elle (Kingsbury and Alvaro, VLDB 2020) checks a history of transactions for the anomalies that each isolation level forbids.

From M5, `rupg-compat jepsen` runs Elle workloads against a single rupg server at each isolation level, through a real client. `SERIALIZABLE` must show no anomaly. `REPEATABLE READ` and `READ COMMITTED` must show only the anomalies that the same workload shows on the oracle. 

From M10, the Jepsen test runs on 3 to 16 nodes with partitions, process kills, process pauses, clock skew and membership changes. The workloads are list-append, register, bank (a total that must not change) and the TPC-C consistency conditions. A Jepsen failure blocks M10 and M11.

## 21.13 Resource tests

**An idle session is at most 64 KiB.** Document 06 section 6.4 sets this limit. The test opens 10,000 sessions that authenticate and wait. It divides the change in the cgroup's `memory.current` by 10,000 and also reads the engine's accounting for each session. Both numbers must be at most 64 KiB. The test repeats after each session has prepared and dropped a statement.

**The memory limit holds.** The test sets `rupg.memory_limit` to 64 MiB and runs TPC-H SF1 and ClickBench on a 10 million row sample. `memory.peak` of the cgroup must stay below the limit plus a fixed allowance for the code and the stacks. Document 19 sets the allowance. The answers must not change. The library build runs the same test in process with its default limit of 256 MiB.

**No leak over time.** A 12 hour pgbench run must end with resident memory at most 1 percent above the second hour. 

**The file does not grow without data.** A test updates the same rows for 1 hour. The file size must become stable, so that out-of-place writes (document 08 section 8.6) reuse their space.

## 21.14 Performance regression

**Per change: instructions retired.** On a dedicated Linux x86-64 runner, CI runs a fixed set of queries with `perf stat -e instructions` and records the count for each query, as the rudb `tenx` work does. The set is the 43 ClickBench queries on a 1 million row sample, the 22 TPC-H queries at SF0.1, 1,000 YCSB point reads, and 100 TPC-C new-order transactions. A change fails when it raises the instructions of one query by more than 3 percent or the total by more than 1 percent. Both thresholds are budgets. A change may pass them on purpose with a reason in the commit message.

**Per change: allocations.** The same run counts the allocations of a point read, a point write and a prepared statement execution. A point read must make no heap allocation after the plan cache is warm. This protects gate G4 from day to day.

**Nightly: time.** Each night `rupg-bench` runs ClickBench, TPC-H SF1 and SF10, YCSB and TPC-C on `gpc`, and each week ClickBench on `c6a.4xlarge` with the rules of document 20. A slowdown over 5 percent, a budget, opens a bug at the commit that `bisect` finds.

**At a milestone: the gate.** The gates of document 02 section 2.10 run with the full rules of document 20.

## 21.15 Publication and the ratchet

Each nightly run writes `rupg-compat/reports/<date>/` with one table for each level.

| Level | Number published |
|---|---|
| L1 Connect | the share of trace messages equal to the oracle, for each of the 48 clients and for `libpq_pipeline` |
| L2 Introspect | the share of rows equal to the oracle, for each of the 64 catalogs, 86 system views and 65 `information_schema` views |
| L3 Query | the pass count of the 239 regression tests and the count of differential cases equal to the oracle |
| L4 Behave | the pass count of the 135 isolation specs and of the Hermitage cases |
| L5 Drop-in | the pass rate of each client suite, against rupg and against the oracle |

The report repeats the tables for each shim version and names every commit and client version.

The file `ratchet.toml` in `rupg-compat` holds the best number of each row. `rupg-compat report` fails when a number is below its ratchet. A number may go down only with an entry in document 24 that says why, for example a pin change that adds tests. `rupg-bench` has the same file for section 21.14.

## 21.16 The CI matrix

The tiers come from document 04 section 4.10. Tier 1 runs every per-change job. Tier 2 runs every job each night.

| Job | Linux x86-64 | Linux aarch64 | macOS aarch64 | Windows x86-64 and wasm32 |
|---|---|---|---|---|
| fmt, `cargo xtask layers`, `cargo xtask style`, docs, MSRV | per change | | | |
| clippy, T1 and T2 in debug and release, T8 subset | per change | per change | nightly | nightly from M9 (wasm32: library only) |
| T3 subset, T10 loom, T13 instructions | per change | | | |
| T3 to T6 full, T12, reproducible build | nightly | nightly | | |
| T8 full, T9, Miri, TSan, ASan | nightly | | | |
| T7 fuzzing | continuous | | | |
| T11 Jepsen | weekly from M5 | | | |

The per-change jobs must finish in 30 minutes, which is a budget. The jobs are in `ci.yml`, `nightly.yml` and `release.yml`, as in rudb. The reproducible build job builds the release binaries twice in different directories and compares the bytes.

## 21.17 What good looks like

At each milestone, the checklist of document 23 must pass. At 1.0 these statements must all be true, and the release report must show the evidence for each one.

1. No known wrong-answer bug, no known lost commit and no known corrupt file.
2. Each fuzz target has run 30 days with no new find.
3. Each of the 239 regression tests, the 135 isolation specs, the 9 `libpq_pipeline` tests and the TAP subset gives the oracle's result on 19 and on each shim version (gate G12).
4. Each of the 48 client suites passes at the oracle's rate.
5. Every suite gives the same answers with every summary deleted (section 21.8).
6. The crash enumeration and the real-kernel crash tests find no lost commit across the counts of section 21.9.4.
7. Jepsen on 3 to 16 nodes finds no anomaly at `SERIALIZABLE`.
8. Each gate of document 02 section 2.10 has a measured number in a published report.

A suite can show that a bug exists, not that no bug exists. These statements say that the suites found none after a stated effort, and the release report states the effort in hours and cases.
