# 16. Procedures and extensions

Written 7 October 2026. Counts are taken at the pin, PostgreSQL `REL_19_STABLE` at `7d3d2db7` (19beta4). Line counts are `wc -l` of the file at the pin.

This document specifies the code that users put inside the database: SQL functions and procedures, PL/pgSQL, triggers, event triggers and rules. It also specifies extensions: how `CREATE EXTENSION` works when rupg does not load C code, which extensions rupg ships, why there is no C ABI, and the two APIs that replace it, one in Rust and one in WebAssembly. Applications depend on this code more than benchmarks show. HammerDB runs TPC-C as PL/pgSQL procedures (document 20). Migration tools create triggers. Most production schemas have at least one extension. The PostgreSQL facts come from the pin and from `../2140/compat/postgres/12-functions-and-operators.md`.

## 16.1 Scope and crates

| Crate | Rank | Contents |
|---|---|---|
| `rupg-func` | 10 | The built-in functions of document 07 section 7.18, and the call interface that every other function kind uses |
| `rupg-plpgsql` | 11 | The PL/pgSQL parser, compiler and interpreter, trigger execution, event trigger execution |
| `rupg-ext` | 11 | The extension catalog, `CREATE EXTENSION`, the binding table of section 16.7, the Rust API of section 16.11, the WebAssembly host of section 16.12 |
| `rupg-contrib` | 11 | The extensions of section 16.8, in Rust |

SQL functions and rules need no crate of their own. A SQL function body goes through `rupg-sql`, `rupg-analyze` and `rupg-plan` like any statement. Rules are part of the rewriter in `rupg-analyze` (document 07 section 7.10). The line budgets of the three rank 11 crates are in document 22.

`rupg-plpgsql`, `rupg-ext` and `rupg-contrib` contain no `unsafe` code. The WebAssembly host uses `wasmtime`, which has its own `unsafe` code behind a safe API, as every dependency of rupg may.

## 16.2 SQL functions and procedures

### 16.2.1 Bodies

A SQL function has one of two body forms. The string form, `AS $$ ... $$`, is stored as text and parsed at each first call in a session, so it can refer to objects that do not exist yet when the function is created. The standard form, `BEGIN ATOMIC ... END` or `RETURN expr`, is parsed and analyzed at `CREATE FUNCTION`. PostgreSQL stores the analyzed tree in `pg_proc.prosqlbody` and records a dependency on every object that the body uses, so `DROP TABLE` of a used table fails without `CASCADE`. rupg does the same. It stores the analyzed tree in its own form, and `pg_get_functiondef` gives the text of the oracle (document 07 section 7.17).

`check_function_bodies` controls whether the string form is checked at create time. pg_dump sets it to `off`, so a restore does not fail on functions that refer to objects created later in the dump. rupg follows it.

### 16.2.2 Inlining

PostgreSQL replaces a call of a simple SQL function with its body, at plan time, in `inline_function` and `inline_set_returning_function` in `optimizer/util/clauses.c`. A scalar function is inlined when all of these are true.

1. The language is `sql`, the function returns one value, and it is not `SECURITY DEFINER`.
2. The function has no `SET` clause and no `prosupport` function that replaces it.
3. The body is one `SELECT` of one expression with no `FROM`, no aggregate, no window function and no sublink to a set.
4. The result of the body has the declared type, or can be coerced to it without a function call.
5. The body is not more volatile than the declaration, and it is strict if the declaration is strict.
6. Each argument with a volatile expression, or with a cost above ten times the cost of an operator, is used at most once in the body.

A set-returning function in `FROM` is inlined when its body is one `SELECT` and the function is not volatile. The plan then has the query of the body as a subquery, and the predicates of the outer query can move into it.

rupg applies the same rules. The plan is not compared (document 05 section 5.3, item 1), but the rules have visible effects: an inlined function does not appear in `pg_stat_user_functions`, a volatile argument is evaluated once and not twice, and an error inside an inlined body has the context of the outer statement. Analytic users write SQL functions as macros, so inlining is also a performance rule: after inlining, the expression is one more vector kernel and not a call for each row.

### 16.2.3 Plans of SQL functions

A SQL function that is not inlined runs its statements with plans from the plan cache of document 06 section 6.9.2, as PostgreSQL 18 changed SQL functions to do. The cache key has the function OID and the argument types, so the plan is shared by every session of the node.

### 16.2.4 Procedures

`CREATE PROCEDURE` makes a `pg_proc` row with `prokind = 'p'`. `CALL` runs it. A procedure can have `OUT` arguments, which `CALL` returns as one row. A procedure called by `CALL` at the top level, or from a chain of `CALL` and `DO` with no function between, can commit and roll back (section 16.3.8). `create_procedure.sql` has 289 lines and `transactions.sql` has 644 lines.

## 16.3 PL/pgSQL

### 16.3.1 The size

PL/pgSQL at the pin has these source files in `src/pl/plpgsql/src`.

| File | Lines | Contents |
|---|---|---|
| `pl_gram.y` | 4,255 | The grammar |
| `pl_exec.c` | 9,226 | The interpreter |
| `pl_comp.c` | 2,351 | Compilation, variable lookup, the function cache |
| `pl_funcs.c` | 1,694 | Utility functions and the tree dump |
| `pl_scanner.c` | 657 | The lexer on top of the core scanner |
| `pl_handler.c` | 550 | The call handler, the validator, the settings |
| Total | 18,733 | |

The language has 23 reserved and 86 unreserved keywords. The tests are `plpgsql.sql` in the main suite, with 4,756 lines, and 13 files in `src/pl/plpgsql/src/sql`: `plpgsql_array`, `plpgsql_cache`, `plpgsql_call`, `plpgsql_control`, `plpgsql_copy`, `plpgsql_domain`, `plpgsql_misc`, `plpgsql_record`, `plpgsql_simple`, `plpgsql_transaction`, `plpgsql_trap`, `plpgsql_trigger` and `plpgsql_varprops`. `triggers.sql` (2,815 lines) and `event_trigger.sql` (645 lines) are mostly PL/pgSQL. The estimate in `../2140/compat/postgres/01-what-compatible-means.md` section 1.7 is that 40 to 50 percent of the regression tests need PL/pgSQL, exact `EXPLAIN` output or the describe output of psql. PL/pgSQL is therefore on the path to gate G6.

### 16.3.2 Parsing

PL/pgSQL is parsed with its own grammar, which calls back into the core parser for each embedded SQL statement and expression. rupg vendors `pl_gram.y` with the same `cargo xtask pg-grammar` build as the core grammar (document 07 section 7.2): the actions are removed, the build generates LALR(1) tables and fails on any conflict, and every production must have a Rust action. The lexer reuses the core lexer of document 07 section 7.3 with the keyword lists of `pl_reserved_kwlist.h` and `pl_unreserved_kwlist.h`.

Embedded SQL is found the way `pl_gram.y` finds it: the text up to the terminating token is one SQL statement or expression. The text is kept, and each embedded statement is parsed by the core parser at compile time. The parse errors and their positions inside the function body must match the oracle, including the `CONTEXT` line `PL/pgSQL function f(integer) line 3 at SQL statement`.

### 16.3.3 Compilation

A function is compiled at its first call in a node, not in each session. The compiled form is shared and immutable, and it is keyed by the function OID, the actual argument types for polymorphic functions, the trigger relation for trigger functions, and the catalog version at which the function body last changed. PostgreSQL keeps one compiled copy in each backend. rupg keeps one in each node. With 1,000 sessions that call the same 50 functions, this removes the compile work and the memory of 999 copies.

The compiled form is an IR of typed slots and instructions.

1. Each variable, record and row variable is a slot with a type, a typmod and a collation. `%TYPE` and `%ROWTYPE` are resolved at compile time and recorded as dependencies.
2. Each control statement (`IF`, `CASE`, `LOOP`, `WHILE`, `FOR`, `FOREACH`, `EXIT`, `CONTINUE`, `RETURN`) is an instruction with jump targets.
3. Each embedded SQL statement is a reference to an entry in the plan cache with its parameter slots. The planner sees PL/pgSQL variables as parameters, with `plpgsql.variable_conflict` deciding between a column and a variable of the same name.
4. Each expression is either a simple expression of section 16.3.4 or a query.

Session state, such as the values of variables and open cursors, is held in the frame of the call, not in the compiled form.

### 16.3.4 Simple expressions

Most PL/pgSQL statements are assignments and conditions such as `i := i + 1` and `IF n > 0 THEN`. PostgreSQL detects that an expression is "simple" (no table, no aggregate, no set function, one row) and evaluates it without the executor in `exec_eval_simple_expr`. rupg does the same. A simple expression compiles to a scalar program over the slots of the frame: the same function kernels as the executor, called with a vector length of 1, with no plan, no snapshot and no allocation. The plan cache entry of the expression is still checked against the catalog version, so `ALTER FUNCTION` of a called function makes the expression compile again.

An expression is simple at the same times as in PostgreSQL, because the difference is visible. A simple expression does not take a new snapshot, and it runs inside the snapshot of the calling statement.

### 16.3.5 Plans and invalidation

The plan of an embedded statement follows `plan_cache_mode`. With `auto`, the first five executions use custom plans with the actual parameter values. After that, a generic plan is used if its cost is not much higher than the average custom plan. rupg keeps the rule and the count of five, because `plpgsql_cache.sql` and user code observe the effect through the visible behavior of statements such as a `SELECT` on a partitioned table.

A plan is invalidated by the catalog version rule of document 07 section 7.11.3. When a table that a function uses gets a new column, the next call plans again, and a record variable with `%ROWTYPE` gets the new shape.

Dynamic SQL, `EXECUTE format(...) USING ...`, is parsed and planned at each execution, as in PostgreSQL. The plan cache keys it by text, so a repeated dynamic statement still reuses its plan.

### 16.3.6 Exceptions

A `BEGIN ... EXCEPTION WHEN ... END` block sets a savepoint at its start. When an error occurs inside the block, the transaction rolls back to the savepoint, the variables keep the values they had at the moment of the error, and the first handler whose condition matches runs. rupg uses the subtransaction of document 11 for the savepoint. The cost of a block with an exception clause comes from that savepoint, so the subtransaction must be cheap when nothing in the block writes. Document 11 specifies a savepoint that costs nothing until the first write.

The condition names are the 268 names of `errcodes.txt` at the pin, plus `OTHERS`, plus `SQLSTATE 'xxxxx'`. `GET STACKED DIAGNOSTICS` gives `RETURNED_SQLSTATE`, `MESSAGE_TEXT`, `PG_EXCEPTION_DETAIL`, `PG_EXCEPTION_HINT`, `PG_EXCEPTION_CONTEXT`, `COLUMN_NAME`, `CONSTRAINT_NAME`, `PG_DATATYPE_NAME`, `TABLE_NAME` and `SCHEMA_NAME`. `GET DIAGNOSTICS` gives `ROW_COUNT`, `PG_CONTEXT` and `PG_ROUTINE_OID`. The context text is compared with the oracle, so the interpreter keeps the call stack with the line number and the statement kind of each frame.

### 16.3.7 Cursors and result sets

PL/pgSQL has bound and unbound cursors, `OPEN ... FOR query`, `OPEN ... FOR EXECUTE`, `FETCH` with every direction, `MOVE`, `CLOSE`, `FOR rec IN cursor LOOP`, and functions that return a `refcursor` to the client. A cursor that the function returns is a portal of the session, so the client can `FETCH` from it after the function returns. rupg uses the portals of document 06.

`RETURN NEXT` and `RETURN QUERY` add rows to the result of a set-returning function. PostgreSQL collects them in a tuplestore and returns them when the function ends. rupg collects them in vectors of 1024 rows and spills to temporary pages of the buffer manager when the memory of the query passes its budget (document 04 section 4.6). The caller reads them as a scan.

A `FOR` loop over a query fetches 1024 rows at a time from the executor, not 10 rows as PostgreSQL does. `exec_for_query` fetches 10 rows at a time, and 1 row at a time in a procedure that can commit. The loop body still sees one row at a time. The difference is not visible, except that a function that modifies the table it loops over sees the rows of the snapshot of the query, which is the same rule in both.

### 16.3.8 Transaction control

A procedure called by `CALL`, and a `DO` block, can run `COMMIT` and `ROLLBACK`, and `COMMIT AND CHAIN` and `ROLLBACK AND CHAIN`. This is allowed only when the chain of calls from the top level has only `CALL` and `DO`. A function, a `SELECT` or an explicit transaction block in the chain makes the command fail with `2D000` `invalid transaction termination`. After a commit, a new transaction starts at once with the default characteristics, or the same characteristics for `AND CHAIN`. A `FOR` loop over a query keeps its cursor open over a commit, as PostgreSQL does by converting the portal to a holdable one. rupg materializes the remaining rows in the same way.

`SET TRANSACTION` inside a procedure, savepoint commands and transaction control in a block with an exception handler fail with the errors of the pin.

### 16.3.9 Messages, assertions and settings

`RAISE` has the levels `DEBUG`, `LOG`, `INFO`, `NOTICE`, `WARNING` and `EXCEPTION`, the `USING` options `MESSAGE`, `DETAIL`, `HINT`, `ERRCODE`, `COLUMN`, `CONSTRAINT`, `DATATYPE`, `TABLE` and `SCHEMA`, and `%` placeholders. A message below `client_min_messages` is not sent. `ASSERT` raises `P0004` when `plpgsql.check_asserts` is on.

PL/pgSQL defines five settings in `pl_handler.c`, and rupg defines the same five with the same context and defaults.

| Setting | Meaning |
|---|---|
| `plpgsql.variable_conflict` | `error`, `use_variable` or `use_column` when a name is both a variable and a column |
| `plpgsql.print_strict_params` | Add the parameter values to the error of `INTO STRICT` |
| `plpgsql.check_asserts` | Run `ASSERT` statements |
| `plpgsql.extra_warnings` | Extra compile checks that give warnings |
| `plpgsql.extra_errors` | Extra compile checks that give errors |

The extra checks are `shadowed_variables`, `strict_multi_assignment` and `too_many_rows`. They are part of the 438 settings of document 06 section 6.13, because `pg_settings` shows them after `plpgsql` is loaded, and in PostgreSQL `plpgsql` is always loaded by its first use. In rupg they exist from the start of each session.

### 16.3.10 Checks at create time

`CREATE FUNCTION ... LANGUAGE plpgsql` runs the validator when `check_function_bodies` is on. The validator parses the body and reports syntax errors. It does not check that tables and columns exist, because PostgreSQL does not. rupg reports exactly the errors that the oracle reports at create time, and no more. A stricter check would be useful, and it would break dumps that create functions before their tables. Users who want it can install `plpgsql_check`, which section 16.9 lists as not planned.

### 16.3.11 A compiled tier

The interpreter is the first tier. A later tier compiles hot functions to machine code with the compile tiers of document 14, after the IR of section 16.3.3 is stable and the regression tests pass. This tier is not a goal for M7. It is planned for M8 only if the TPC-C measurement of document 20 shows that interpreter time is a large share of the New-Order CPU time.

## 16.4 Triggers

### 16.4.1 The surface

rupg supports the trigger surface of the pin.

| Feature | Notes |
|---|---|
| Row and statement triggers | `FOR EACH ROW` and `FOR EACH STATEMENT` |
| `BEFORE`, `AFTER` and `INSTEAD OF` | `INSTEAD OF` on views only |
| Events | `INSERT`, `UPDATE`, `UPDATE OF columns`, `DELETE`, `TRUNCATE` |
| `WHEN` conditions | Evaluated before the function is called |
| Transition tables | `REFERENCING OLD TABLE AS ... NEW TABLE AS ...` on `AFTER` triggers |
| Constraint triggers | `CREATE CONSTRAINT TRIGGER`, `DEFERRABLE`, `INITIALLY DEFERRED`, `SET CONSTRAINTS` |
| Partitioned tables | A trigger on the parent is cloned to each partition, and `pg_trigger` shows the clones with `tgparentid` |
| `ALTER TABLE ... ENABLE` and `DISABLE TRIGGER` | With `ENABLE REPLICA` and `ENABLE ALWAYS` |
| `MERGE` | Fires the triggers of the actions that it runs |

The trigger function gets the PostgreSQL special variables: `NEW`, `OLD`, `TG_NAME`, `TG_WHEN`, `TG_LEVEL`, `TG_OP`, `TG_RELID`, `TG_TABLE_NAME`, `TG_TABLE_SCHEMA`, `TG_NARGS` and `TG_ARGV`. A `BEFORE` row trigger that returns `NULL` skips the row. One that returns a changed `NEW` changes the row. 17 functions in `pg_proc.dat` return `trigger`, and they are built in.

### 16.4.2 Order

Triggers of the same timing, level and event fire in alphabetical order of their names, as in PostgreSQL. Applications depend on this order and name their triggers to control it. The order of `BEFORE` row triggers, the constraint checks, the foreign key checks and `AFTER` triggers is the order of the pin.

### 16.4.3 How row triggers run on vectors

The executor of document 14 handles rows in vectors of 1024. A table with no row trigger keeps that path. A table with `BEFORE` row triggers runs the trigger function once for each row of the vector, in row order, and builds the output vector from the rows that the function returns. A `WHEN` condition is evaluated as a vector filter first, so the function runs only for the rows that pass. A trigger written in Rust through the API of section 16.11 gets the whole vector.

`AFTER` row triggers are queued with the row ids and the changed columns, not the full rows, and run at the end of the statement, or at commit for deferred constraint triggers. The queue spills to temporary pages when it passes its share of the query memory. A bulk `UPDATE` of 100 million rows with an `AFTER` trigger therefore does not run out of memory. PostgreSQL also keeps its after-trigger queue in memory and does not spill it.

**Cold rows.** A row in the cold store of document 10 has no row image to point to. When an `UPDATE` or `DELETE` with a row trigger touches a cold row, rupg builds `OLD` from the column segments for the columns that the trigger can read. When the function reads `OLD` as a whole record, every column is built.

### 16.4.4 Foreign keys

PostgreSQL implements foreign keys with internal triggers: two check triggers on the referencing table and two action triggers on the referenced table, named `RI_ConstraintTrigger_c_N` and `RI_ConstraintTrigger_a_N`. rupg checks foreign keys in the executor, a vector of keys at a time against the index of the referenced table, which is much faster than one query for each row. It also creates the four `pg_trigger` rows with `tgisinternal` true and the same names, functions and flags as PostgreSQL. Tools read them, and the rows control behavior that users rely on.

1. `ALTER TABLE t DISABLE TRIGGER ALL` disables the foreign key checks of `t`, as in PostgreSQL. Only a superuser can do this.
2. `session_replication_role = replica` disables the triggers that are enabled in `ORIGIN` mode, which includes the foreign key triggers. Logical replication apply and some data loaders use this setting. A trigger with `ENABLE REPLICA` fires only in replica mode, and one with `ENABLE ALWAYS` fires in both.
3. The point at which a deferred foreign key is checked follows `SET CONSTRAINTS`.

The foreign key checks also run at the same point in the trigger order as the internal triggers would run, so a `BEFORE` trigger that changes a key value is seen by the check.

### 16.4.5 COPY and triggers

`COPY FROM` fires the row and statement triggers of the table. The bulk path of document 06 section 6.11 writes vectors directly only when the table has no `BEFORE` row trigger and no `INSTEAD OF` trigger. With such a trigger, `COPY` runs the trigger for each row and then writes the vector. `AFTER` row triggers and foreign key checks do not stop the bulk path, because they are queued.

## 16.5 Event triggers

An event trigger fires on DDL and on login. The pin has five events.

| Event | Fires |
|---|---|
| `ddl_command_start` | Before a DDL command runs |
| `ddl_command_end` | After a DDL command runs, before commit. `pg_event_trigger_ddl_commands()` returns the objects that the command made or changed |
| `sql_drop` | After a command drops objects. `pg_event_trigger_dropped_objects()` returns them |
| `table_rewrite` | Before a command rewrites a table, with `pg_event_trigger_table_rewrite_oid()` and `pg_event_trigger_table_rewrite_reason()` |
| `login` | When a session starts, from PostgreSQL 17 |

The event trigger function returns `event_trigger`, and one built-in function in `pg_proc.dat` has that return type. The set of commands that fire each event is the table `event-trigger-matrix` of the PostgreSQL documentation, and rupg generates its own table from the command tags of the pin.

**table_rewrite.** PostgreSQL rewrites a table for `ALTER TABLE ... ALTER COLUMN ... TYPE`, for some `ADD COLUMN` forms and for `ALTER TABLE ... SET ACCESS METHOD`. rupg does not always rewrite in these cases, because a column store can change one column without the others. The event still fires for exactly the commands for which the pin fires it, with the same reason code, because tools such as migration checkers use it to warn about a long lock. The value of the event is the warning, not the physical work.

**login.** A `login` event trigger runs in the new session before the first `ReadyForQuery`. An error in it ends the login. PostgreSQL sets the flag `dathasloginevt` in `pg_database` when a login trigger exists, so a session without one costs nothing. rupg keeps the flag, and the connect path of document 06 checks it.

Event triggers do not fire when `event_triggers` is `off`. That setting is new in PostgreSQL 17 and exists so that a superuser can repair a broken trigger.

## 16.6 Rules

A view is a table with a `_RETURN` rule in `pg_rewrite`. That is the rule that the rewriter uses to expand a view, and rupg stores every view in the same way, so `pg_rewrite` has the same rows as the oracle. `CREATE RULE` makes rules of the other kinds: `DO ALSO` and `DO INSTEAD` rules on `INSERT`, `UPDATE` and `DELETE`, with an optional condition and `NOTHING`. The rewriter of document 07 section 7.10 applies them in name order, with `NEW` and `OLD` replaced by the expressions of the original command.

Rules are rare in new code. PostgreSQL keeps them, and `rules.sql` (1,462 lines) is in the regression suite. Most of its expected output is the text of `pg_rules` and `pg_views` for every system view and rule, so it is a test of the deparser of document 07 section 7.17 more than of rules. rupg implements rules completely, because the rule surface is small and a rule that is half done gives wrong data, not an error.

## 16.7 The extension model

### 16.7.1 What an extension is in PostgreSQL

A PostgreSQL extension is a control file, one or more SQL scripts, and usually a shared library. `CREATE EXTENSION name` reads the control file, runs the install script for the default version, and records every object the script makes in `pg_depend` with type `e`, so `DROP EXTENSION` can remove them. The scripts declare C functions with `AS 'MODULE_PATHNAME', 'symbol' LANGUAGE C`, and the server loads the library and finds the symbol at the first call. `ALTER EXTENSION ... UPDATE` runs the update scripts from the installed version to the target version. pg_dump writes `CREATE EXTENSION` and not the objects.

### 16.7.2 What rupg does

rupg keeps the same model with one change: the library is not a file. Each extension that rupg ships has its control file and its install and update scripts vendored from the pin, unchanged. The C symbols that the scripts name are bound to Rust functions in a static table.

1. `CREATE EXTENSION name` finds the control file in the built-in set, then in the WebAssembly modules stored in the file (section 16.12), then in the directories of `extension_control_path`.
2. It runs the install script and the update scripts in the order that PostgreSQL chooses.
3. When the script runs `CREATE FUNCTION ... LANGUAGE C` with the module path of a built-in extension, rupg looks up the pair of the module name and the symbol in the binding table. If the pair is bound, the function is made with `prolang` 13 (`c`), `probin` `$libdir/name` and `prosrc` the symbol, as on the oracle, and a call goes to the Rust function.
4. If the pair is not bound, the command fails with `0A000` and the text `C functions are not supported`, with a detail that names the module and the symbol.
5. A `CREATE FUNCTION ... LANGUAGE C` by a user fails in the same way, unless the module and symbol are bound. `LOAD` fails with `0A000`.

So `pg_proc`, `pg_extension` and `pg_depend` show the same rows as on the oracle, and pg_dump of a rupg database restores on PostgreSQL with the same extensions installed. The objects that the scripts make are normal rows of the catalog inside the file (document 07 section 7.11), so an extension needs no file outside the `.rupg` file after it is installed.

### 16.7.3 Trust and privileges

A control file with `trusted = true` lets a user with `CREATE` on the database install the extension, and the script then runs as the bootstrap superuser. At the pin, 22 of the 55 contrib control files are trusted. rupg keeps the flag of each control file. `superuser` and `relocatable` and `schema` and `requires` work as in PostgreSQL.

### 16.7.4 The catalogs

`pg_available_extensions` and `pg_available_extension_versions` list the built-in set, the stored WebAssembly modules and the extensions found in `extension_control_path`. PostgreSQL 19 adds the `location` column, which gives the directory of the control file. For a built-in extension, rupg shows `$system`, as PostgreSQL does for its own directory. A user who is not a superuser sees `<insufficient privilege>`, as on the oracle. `pg_extension` and `pg_extension_config_dump` work as in PostgreSQL, so pg_dump dumps the configuration tables of an extension.

### 16.7.5 SQL-only extensions on disk

PostgreSQL 18 added `extension_control_path`, a list of directories that are searched for control files. rupg honors it in the server for extensions whose scripts have no unbound C function. Many extensions are SQL and PL/pgSQL only, for example `pgjwt` and the SQL part of `pg_partman`. After `CREATE EXTENSION`, the objects live in the file, so the file stays complete. `ALTER EXTENSION ... UPDATE` needs the scripts again. In the library and in the WebAssembly build, the setting is accepted and ignored, because the embedding program decides which files the database can read.

### 16.7.6 The regression library

The regression suite calls C functions from `src/test/regress/regress.c`, which the pin builds as `$libdir/regress`. `test_setup.sql` creates `binary_coercible` from it, and 17 test files refer to the library. `regress.c` has 36 functions with `PG_FUNCTION_INFO_V1`. rupg implements these functions in Rust and binds them under the module `regress`, in a cargo feature `pg-regress` that only `rupg-compat` builds. A release build does not have the feature. With it, those tests are counted in L3 and not excluded, which narrows the exclusion rule of document 05 section 5.8. A function of `regress.c` that tests a C internal with no meaning in rupg is listed in `rupg-compat` with its tests as excluded.

## 16.8 The built-in extensions

Each extension in this table is in `rupg-contrib`, behind a cargo feature with its name, and on by default in the server. The version is the default version of the control file at the pin. The extension counts toward L3 when its `contrib` regression tests pass (document 05 section 5.11).

| Extension | Version | Trusted | Milestone | Notes |
|---|---|---|---|---|
| `pg_trgm` | 1.6 | yes | M6 | Similarity functions, `%` operators, GIN and GiST operator classes. The GIN classes use the index of document 12 |
| `btree_gin` | 1.4 | yes | M6 | GIN operator classes for scalar types |
| `btree_gist` | 1.9 | yes | M6 | GiST operator classes for scalar types, needed for exclusion constraints and `WITHOUT OVERLAPS` (document 12) |
| `vector` (pgvector) | at M6 | yes | M6 | The `vector`, `halfvec`, `sparsevec` and `bit` surface, the distance operators, and the `hnsw` and `ivfflat` access methods mapped to the vector index of document 13 |
| `uuid-ossp` | 1.1 | yes | M7 | `uuid_generate_v1` to `v5`. `gen_random_uuid` and `uuidv7` are core |
| `pgcrypto` | 1.4 | yes | M7 | Hashes, HMAC, `crypt` with `bf`, `md5`, `xdes` and `des`, PGP symmetric and public key functions, `gen_random_bytes`. The crypto comes from the same provider as TLS (document 06 section 6.6) |
| `citext` | 1.8 | yes | M7 | A case-insensitive text type |
| `hstore` | 1.8 | yes | M7 | The key and value type, its operators and its GIN and GiST classes |
| `ltree` | 1.3 | yes | M7 | Label trees, `lquery`, `ltxtquery`, GiST classes |
| `intarray` | 1.5 | yes | M7 | Integer array operators and classes |
| `cube` | 1.5 | yes | M7 | |
| `earthdistance` | 1.2 | no | M7 | Requires `cube` |
| `tablefunc` | 1.0 | yes | M7 | `crosstab`, `normal_rand`, `connectby` |
| `fuzzystrmatch` | 1.2 | yes | M7 | `levenshtein`, `soundex`, `metaphone`, `dmetaphone`, `daitch_mokotoff` |
| `unaccent` | 1.1 | yes | M7 for the function, M12 for the text search dictionary | The rules file is vendored and compiled in |
| `pg_stat_statements` | 1.13 | no | M7 | Fed by the plan cache and the executor counters. The values of `queryid` and of the counters are not compared (document 05 section 5.3, item 3) |
| `pg_buffercache` | 1.7 | no | M8 | An emulation over the buffer manager of document 08. The columns are the same. The values describe rupg frames |
| `postgres_fdw` | 1.3 | no | M9 | Foreign tables on a PostgreSQL server, with the client side of `rupg-wire`. Predicate, join and aggregate pushdown |
| `pg_cron` | at M9 | no | M9 | Not contrib. The `cron` schema, `cron.schedule`, `cron.job` and `cron.job_run_details`, run by a task of the node. In a cluster, one node runs each job |

`shared_preload_libraries` is accepted and has no effect, because every built-in extension is present from the start. An application that sets it for `pg_stat_statements` or `pg_cron` works.

The other contrib extensions are in three groups.

1. **M12, the long tail.** `isn`, `seg`, `lo`, `intagg`, `dict_int`, `dict_xsyn`, `tsm_system_rows`, `tsm_system_time`, `tcn`, `autoinc`, `insert_username`, `moddatetime`, `refint`, `sslinfo`, `file_fdw` (server only), `dblink`, `bloom`, `xml2`, `pgrowlocks` and `pg_stash_advice`.
2. **Replaced by a rupg form.** `amcheck`, `pageinspect`, `pg_visibility`, `pg_freespacemap`, `pg_surgery`, `pg_walinspect`, `pg_logicalinspect`, `pgstattuple` and `pg_prewarm` inspect PostgreSQL's heap pages, index pages, visibility map and WAL. rupg has none of these. The file check of document 08 replaces `amcheck`. The others are not provided, and `CREATE EXTENSION` fails with `0A000`.
3. **Never.** The `*_plperl`, `*_plperlu` and `*_plpython3u` transform extensions need a language that rupg does not have (section 16.13).

The pgvector and pg_cron versions are not in the pin. Each is recorded in the vendor file of its extension when its milestone starts, and the pgvector test suite of that version is the test.

## 16.9 Other extensions

These are the extensions outside contrib that users ask for most, with the decision for each.

| Extension | Decision | Reason |
|---|---|---|
| PostGIS | Deferred, not before M12 | It is the most used extension outside contrib. It depends on GEOS, PROJ and GDAL, which are large C and C++ libraries, and its surface is more than 1,000 functions. A Rust port is a project of its own. Document 24 holds the decision |
| pgRouting | With PostGIS | Depends on PostGIS |
| TimescaleDB | No | Most of its features are under the Timescale License, not an open source license. Its hypertables and its compression duplicate partitioning and the column store of rupg |
| Citus | No | rupg shards with its own design (document 18) |
| pg_partman | Works when its scripts run | The partition logic is SQL and PL/pgSQL. Its background worker is C, and `pg_cron` replaces it |
| pg_repack | Replaced | PostgreSQL 19 has `REPACK`, which rupg supports. The rupg form compacts the table in the file (document 10) |
| pgjwt | Works | SQL only, on top of `pgcrypto` |
| pgaudit | Not planned before M12 | rupg has its own audit log design in document 19 if one is needed. The settings of pgaudit are not emulated |
| plpgsql_check | Not planned | It reads the internal structures of PL/pgSQL. A rupg form could use the IR of section 16.3.3 |
| pg_hint_plan | Not planned | Hints name plan nodes of PostgreSQL. rupg plans differ by design (document 15) |
| ParadeDB `pg_search` | Not ported | It is under the AGPL, and this design does not read its source. Its published behavior, BM25 search with Tantivy, is a reference for the text search work of M12 |

**pgrx extensions.** Many new extensions are written in Rust with pgrx. pgrx 0.18 supports PostgreSQL 13 to 18. A pgrx extension is a shared library that uses the C ABI of the server through bindings, so rupg cannot load it either (section 16.10). Its Rust code is often a good start for a port to the API of section 16.11, when its license allows.

## 16.10 Why there is no C ABI

A PostgreSQL extension in C does not use a small plugin interface. It calls the internal functions of the server, and it depends on the data structures of the server.

| Dependency | What the extension expects |
|---|---|
| Memory | `palloc` and memory contexts, freed when the context resets |
| Function calls | `fmgr` with `FunctionCallInfo`, `PG_GETARG_*` and `PG_RETURN_*` macros, one row for each call |
| Values | `Datum`, a pointer-sized word, with varlena headers and TOAST pointers that the extension detoasts |
| Rows | `HeapTuple` and the heap page layout, `TupleDesc`, `slot_getattr` |
| Queries | SPI, which runs SQL from inside a function with the executor of the server |
| Catalog | The syscache and the relcache, `SearchSysCache` with the C structs of each catalog |
| Errors | `ereport` with `longjmp`, `PG_TRY` and `PG_CATCH` |
| Processes | Shared memory segments, lightweight locks, background workers, signal handlers, one process for each session |
| Hooks | Global function pointers such as `ExecutorStart_hook` and `planner_hook` |
| Index methods | `IndexAmRoutine` over buffer pages with the heap `ItemPointer` |

rupg has none of these as PostgreSQL has them. Its values are vectors of 1024 in a columnar layout (document 04), its rows have a 64-bit row id and no heap tuple, its sessions are tasks and not processes (document 06 section 6.2), its errors are Rust results, and it has no global hooks. To load a C extension, rupg would need to emulate all of them: convert each vector to `Datum` values one row at a time, build heap tuples on demand, run SPI on the rupg executor, keep a syscache with the C structs of 64 catalogs, catch `longjmp` across Rust frames, which Rust does not allow, and give each extension a process-like global state. That emulation would be a second database inside rupg. It would be slower than PostgreSQL, because each call would convert its data, and each PostgreSQL release would break it, because these interfaces change in every major version. An extension built for one PostgreSQL major version does not load in another one. pgrx releases support for each new version.

So rupg has no C ABI. Document 02 lists binary compatibility with C extensions as a non-goal. The cost is real: users of an extension that rupg does not ship cannot use it. The plan to reduce the cost is to ship the most used extensions natively (section 16.8) and to give two APIs for new ones.

## 16.11 The Rust extension API

### 16.11.1 The traits

`rupg-ext` defines traits that work on vectors, not on one value at a time.

| Trait | Purpose |
|---|---|
| `ScalarFunction` | Takes argument vectors and a selection, and writes a result vector. Declares strictness, volatility and parallel safety |
| `AggregateFunction` | A state type with `init`, `update` over a vector, `combine` for parallel and distributed aggregation, and `finalize` |
| `WindowFunction` | Called over a partition with frame bounds |
| `TableFunction` | A set-returning function that produces vectors until it ends |
| `TypeIo` | The text input and output, binary receive and send, and the storage form of a new base type |
| `RowTrigger` | Receives a vector of `OLD` and `NEW` rows |
| `IndexMethod` | An index access method over the page API of document 12. Not stable before M12 |

A function gets a context with the memory pool of the query, the session settings and a way to raise a PostgreSQL error with a SQLSTATE. It does not get the catalog, the transaction or the executor. A function that needs to run SQL is written in PL/pgSQL or SQL, not in Rust. This limit keeps the API small, so it can stay stable while the engine changes.

### 16.11.2 Registration

An extension is a Rust value of type `rupg_ext::Extension`. It holds the control file fields, the install and update scripts as text, and the binding table from (module, symbol) to trait objects. It is registered in one of two ways.

1. **In the build.** `rupg-contrib` registers each built-in extension behind its cargo feature. A third party can make its own server binary that depends on `rupg-server` and its extension crate and registers the extension at start. Document 17 shows the builder.
2. **By the embedding program.** A program that uses rupg as a library registers its extensions on the builder before it opens the file.

There are no dynamic libraries. An extension is compiled into the binary with the same Rust compiler and the same rupg version, so there is no ABI to keep stable and no `unsafe` code at the boundary. The API follows semantic versioning with the `rupg` crate.

When a file was created with an extension that the opening binary does not have, the file opens. A call to a function of the missing extension fails with `0A000` and a detail that names the extension. `DROP EXTENSION` works.

## 16.12 The WebAssembly extension API

### 16.12.1 Why

The Rust API needs a new binary. A user of a released server, or of the library inside an application that they did not build, cannot add a Rust extension. WebAssembly modules give a second way: an extension that is data, stored in the database file, that runs in a sandbox and gives the same results on every platform.

### 16.12.2 How

1. The host is `wasmtime`, behind the cargo feature `wasm-ext`. The feature is on in the server and the CLI, and off by default in the library.
2. The interface is a WIT world, `rupg:extension`, in the WebAssembly component model. It covers the same kinds as the Rust API except `IndexMethod`: scalar, aggregate, window and table functions, and type I/O. A call passes one vector, not one value, in the layout of document 04, so the cost of crossing into the module is paid once for each 1024 rows.
3. A superuser stores a module with `SELECT rupg.install_wasm_extension(name, module)`, where `module` is a `bytea`. The module carries its control fields and its SQL scripts in a custom section. rupg validates the module and stores it in the system table `rupg_wasm_module`. `CREATE EXTENSION name` then works as for a built-in extension, and the scripts bind `LANGUAGE C` symbols to exports of the module. `rupg.drop_wasm_extension(name)` removes the module when no installed extension uses it.
4. The module has no WASI by default: no files, no clock, no network, no random numbers. A function that needs the time or random bytes must be declared `VOLATILE`, and the host gives these through its own imports. So an `IMMUTABLE` function of a module gives the same result on every node of a cluster, which replication needs (document 18).
5. Each call has a fuel limit, set by `rupg.wasm_fuel` (default 1,000,000,000 units for each call of a vector), and each instance has a memory limit, set by `rupg.wasm_memory_limit` (default 64 MiB). The memory counts against the memory of the query. A call that runs out of fuel fails with `57014`, as a canceled statement does. A trap fails with `XX000` and a detail with the trap text.
6. A module is compiled when it is installed and when a node opens the file. The compiled code is kept in memory only.

The module is part of the file, so it moves with the file, is backed up with the file and is replicated to every node of a cluster.

### 16.12.3 Limits

`rupg-wasm`, the WebAssembly build of rupg for browsers (document 17), cannot run `wasmtime` inside itself. Document 24 holds the question of whether that build should call modules through the WebAssembly engine of the browser.

## 16.13 Other procedural languages

`pg_language` has the rows `internal`, `c`, `sql` and `plpgsql`, as on a new PostgreSQL cluster. rupg does not ship PL/Perl, PL/Python, PL/Tcl or PL/v8. Each one embeds an interpreter in the server process and calls the C internals of the server. `CREATE LANGUAGE` and `CREATE EXTENSION plperl`, `plpython3u` or `pltcl` fail with `0A000`.

A JavaScript language is the most requested of these after PL/pgSQL. It could be a WebAssembly module of section 16.12 that contains a JavaScript engine. Document 24 holds the question.

## 16.14 Performance

PL/pgSQL is on the path of the TPC-C target of document 02 section 2.7, because HammerDB runs New-Order, Payment, Delivery, Order-Status and Stock-Level as stored procedures by default. The goal of that section is the target. The numbers below are budgets for this document. They are not measurements, and `rupg-bench` measures each one at M7 against the oracle on the same host.

| Path | Budget |
|---|---|
| A simple expression, `i := i + 1` | 30 ns |
| An embedded `SELECT ... INTO` on the point path of document 14 section 14.9 with a cached plan | 1 µs |
| A call of a compiled PL/pgSQL function from SQL, with no statements in the body | 200 ns |
| A `BEFORE` row trigger that sets one column of `NEW`, for each row of a 1024-row `INSERT` | 100 ns |
| An `EXCEPTION` block that enters and leaves with no error and no write | 100 ns |
| The first call of a 200-line function in a node, including compilation | 1 ms |

The shared compiled form of section 16.3.3 is also a resource target. A PostgreSQL backend keeps its own compiled copy of each function it calls, and its own plans for each statement in them. rupg keeps one copy for each node. The memory of the compiled functions is part of the shared catalog cache and not of the 64 KiB session budget of document 06 section 6.4.

## 16.15 Gates

| Milestone | What this document must deliver |
|---|---|
| M3, gate G4 | SQL functions with both body forms, inlining, `CREATE PROCEDURE` and `CALL` without transaction control, `DO` with SQL only |
| M5, gate G5 | Triggers on the hot path with the foreign key rows of section 16.4.4, `session_replication_role` |
| M6 | `CREATE EXTENSION` with the binding table, and the extensions of M6 in section 16.8 |
| M7, gate G6 | PL/pgSQL complete, with the 13 PL/pgSQL test files and `plpgsql.sql`. Triggers complete with transition tables and constraint triggers. Event triggers. Rules. The extensions of M7. The regression library of section 16.7.6. The regression suite at 100 percent of its denominator. The HammerDB procedures run |
| M8, gates G7 to G10 | The budgets of section 16.14 met. `pg_buffercache` |
| M9 | `postgres_fdw`, `pg_cron`, the Rust API at version 1.0, the WebAssembly API, `ENABLE REPLICA` triggers under logical replication apply |
| M12, gate G12 | The long tail of section 16.8. The contrib regression tests at 100 percent for every shipped extension |

## 16.16 Open questions from this document

16.A. Document 22 gives M7 as the first milestone of `rupg-ext` and `rupg-contrib`. Documents 12 and 13 need `btree_gist`, `pg_trgm` and the pgvector surface at M6, and this document puts the extension machinery at M6 for that reason. One of the two must change.

16.B. Should rupg port PostGIS, and when? It is the most used extension that rupg does not ship. The work is a geometry engine in Rust that gives the same answers as GEOS, and the projection data of PROJ.

16.C. Should rupg ship a JavaScript procedural language as a WebAssembly module? It would cover users of PL/v8, and it would test the API of section 16.12 with a large module.

16.D. Should the browser build `rupg-wasm` run WebAssembly extensions through the engine of the browser? The sandbox and fuel rules of section 16.12 would need another implementation.

16.E. Should the compiled code of a WebAssembly module be cached in the file, keyed by the `wasmtime` version and the CPU? This saves the compile time at open, at the cost of platform data inside a portable file.
