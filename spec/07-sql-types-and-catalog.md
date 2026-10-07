# 7. SQL, types and catalog

Written 7 October 2026. Counts are taken at the pin, PostgreSQL `REL_19_STABLE` at `7d3d2db7` (19beta4). Line counts are `wc -l` of the vendored file.

This document specifies the path from statement text to a checked query tree, and the objects that the path reads. It covers the vendored grammar and the lexer, the analyzer with its name, operator, function and cast resolution, collations and time zones, the 197 built-in types and their input and output, the rewriter, the catalog inside the file and its version counter, OID assignment, the virtual `pg_catalog`, `information_schema` and the statistics views, the deparse functions, and how the functions and operators are tested. The planner and the optimizer start where this document ends, in document 15. The facts about PostgreSQL come from `../2140/compat/postgres/06-types-and-oids.md`, `07-the-catalog.md`, `08-the-dialect.md` and `12-functions-and-operators.md`. rupg has one dialect, so the parts of those documents that kept a DuckDB dialect beside PostgreSQL do not apply.

## 7.1 From text to tree

A statement passes through five steps. Each step is in one crate, and each crate is at the rank that document 04 section 4.1 gives it.

| Step | Crate | Rank | Output |
|---|---|---|---|
| Lex and parse | `rupg-sql` | 8 | A raw parse tree in an arena, with the source position of every node |
| Analyze | `rupg-analyze` | 9 | A query tree with resolved names, types, typmods, collations and the source column of each output |
| Rewrite | `rupg-analyze` | 9 | The query tree with views, rules and row security applied |
| Plan and optimize | `rupg-plan`, `rupg-opt` | 9 | A physical plan, document 15 |
| Execute | `rupg-exec` | 10 | Vectors, document 14 |

The types and their input and output functions are in `rupg-types` at rank 0, so every crate can use them. The catalog objects are in `rupg-catalog` at rank 7. The PostgreSQL views of the catalog are in `rupg-pgcatalog` at rank 7. The data of older versions for the shim is in `rupg-shim` at rank 7 (document 05 section 5.6).

The analyzer reads the catalog at the snapshot of the statement, so a statement inside a transaction sees the DDL of that transaction. The plan cache key has the catalog version of section 7.11, so a plan made before DDL is not used after it.

## 7.2 The vendored grammar

**The decision.** rupg parses with the grammar of PostgreSQL, not with a grammar that we write. `gram.y` at the pin has 20,077 lines. Bison 2.3 reports 3,476 rules, 743 nonterminals, 546 terminals and 6,574 states for it. A hand-written grammar of that size drifts from PostgreSQL in its precedence and its corner cases, and each drift is a difference in L3. The rules of `gram.y` are under the PostgreSQL License, so we can vendor them. The semantic actions are C code that builds PostgreSQL nodes, so we write our own actions in Rust.

**The build.** The command `cargo xtask pg-grammar` does these steps.

1. Copy `gram.y`, `scan.l`, `kwlist.h` and `parser.c` from the pin into `crates/rupg-sql/vendor/`. Record the commit and the SHA-256 of each file in `vendor/VENDOR`.
2. Remove every C action block from `gram.y`. Keep the `%token` declarations, the precedence declarations, every rule, every alternative and every `%prec`. Write the result to `vendor/gram.rules`.
3. Give each alternative a stable name, the rule name and the index of the alternative, for example `a_expr.17`. Write the names to `vendor/productions.txt`.
4. Generate LALR(1) tables from `gram.rules`. Fail the build on any conflict.
5. Write the tables as static Rust arrays into `crates/rupg-sql/src/generated/`.
6. Generate a perfect hash over the 499 keywords of `kwlist.h`, with the category of each: 335 unreserved, 78 reserved, 63 column-name and 23 type-or-function-name.
7. Compare `productions.txt` with the productions that have an action in `crates/rupg-sql/src/actions/`. Fail the build if a production has no action.

The table generator is `cfgrammar` and `lrtable` from grmtools, used in `xtask` only. They are not a runtime dependency. If they cannot read a construct of `gram.y`, we write a LALR(1) generator in `xtask` with the DeRemer and Pennello lookahead sets. In both cases, CI runs bison on the same `gram.rules` and compares the number of productions and states with ours.

**The actions.** An action builds a node of the raw parse tree. The raw tree has one node type for each PostgreSQL parse node that a production can build, with the same fields, so the analyzer can follow the PostgreSQL analysis code one function at a time. The nodes live in an arena that is freed when the statement has been analyzed.

**The parse cost.** A table-driven parser does one table lookup for each token. A 100-byte `SELECT ... WHERE id = $1` is about 20 tokens. The prediction in `../2140/compat/postgres/08-the-dialect.md` section 8.14 is 1 to 3 µs for this parse. It is a prediction until `cargo bench` measures it at M3. Prepared statements and the plan cache remove the parse from the repeated path, and the point path of document 14 section 14.9 recognizes its statement shapes from the parse tree without an analyzer pass.

## 7.3 The lexer

The lexer is a hand port of `scan.l` (1,421 lines) into a Rust state machine. It has the same exclusive start states: bit strings, hex strings, quoted strings, extended strings with `E'...'`, dollar-quoted strings, delimited identifiers, identifiers and strings with `U&`, and nested comments. PostgreSQL 19 accepts only `on` for `standard_conforming_strings`, so a plain `'...'` literal never treats a backslash as an escape. Under a shim of 14 to 18 with `standard_conforming_strings = off`, the lexer takes the old path (document 05 section 5.6.3).

The port includes the lookahead filter of `parser.c`. It looks one token ahead and changes `NOT` before `BETWEEN`, `IN`, `LIKE`, `ILIKE` or `SIMILAR` into `NOT_LA`, and does the same for `FORMAT`, `NULLS`, `WITH` and `WITHOUT`. Without the filter, the grammar is not LALR(1).

Identifiers are folded to lower case with the PostgreSQL rule, which folds only ASCII letters in a UTF-8 database. An identifier longer than 63 bytes is truncated with the `NOTICE` of the pin. Names cannot contain a carriage return or a line feed in 19.

The lexer is tested on its own. Every token boundary of the regression suite SQL, 241 files, is compared with the output of the PostgreSQL scanner, which `rupg-compat` gets from the oracle build.

## 7.4 rupg additions to the grammar

rupg adds a small amount of syntax: the property graph syntax of document 13 and nothing else at the date of this document. The additions are in a separate rules file, `crates/rupg-sql/rupg.rules`. The build merges it with `gram.rules` and generates one table. The merged grammar must also have no conflict.

The additions use keywords that PostgreSQL does not have. The lexer returns those keywords only when `rupg.graph_syntax` is on. With the default of off, the lexer returns them as plain identifiers, so every statement parses as it does at the pin and every error has the same position and text. A test parses the regression suite with the setting off and with it on, and the results must be the same for every statement that does not use the new syntax.

`USING link`, `USING vector` and `USING columnar` are not grammar additions. `USING name` is PostgreSQL syntax, and the names are access methods in the catalog.

## 7.5 The analyzer

The analyzer follows the PostgreSQL analysis of `analyze.c` and the `parse_*.c` files, which are 43,110 lines together. It is the largest part of the front end. We port its rules, not its code structure.

**Names and the search path.** An unqualified name is looked up in the effective search path. The effective path is `pg_temp` of the session if it is not listed, then `pg_catalog` if it is not listed, then the schemas of `search_path` in order, where `$user` is the name of the current role and a schema that does not exist is skipped. A qualified name is looked up in its schema only. A table name in `CREATE` goes to the first schema of the path that exists. A function or operator is never looked up in `pg_temp`. These are the rules of `namespace.c`, and catalog queries depend on them through the `pg_*_is_visible` functions.

**Unknown literals.** A quoted literal without a type has type `unknown` until resolution gives it a type from its context. In `SELECT 'a'` with no context, it becomes `text`. In `INSERT`, it takes the type of the target column. In an operator call, it takes the type that the operator resolution chooses.

**Output column names.** An output expression without an alias gets the PostgreSQL name: the column name for a column reference, the function name for a function call, the type name for a cast, and `?column?` for anything else. `SELECT count(*)` gives `count`. `SELECT 1+1` gives `?column?`.

**Source columns and typmods.** The analyzer carries the source table OID and the column number of each output that is a plain column reference, through subqueries and views, to the `RowDescription` of document 06 section 6.10. It carries the typmod with the rules of `exprTypmod`: a column reference keeps the typmod of its column, and most expressions give -1. `SELECT name FROM t` reports 259 for a `varchar(255)` column, and `SELECT name || ''` reports -1.

**Errors.** Each analysis error has the SQLSTATE, the text and the cursor position of the pin. The position is the byte offset of the token that PostgreSQL names, counted in characters as PostgreSQL counts them. Drivers show the position under the statement, so a wrong position is a visible difference.

## 7.6 Operators, functions and casts

**Operators.** An operator is resolved by the algorithm of the section "Operators" of `typeconv.sgml`, implemented in `parse_oper.c`. Look for an exact match. Then treat `unknown` as the other side's type. Then drop the candidates that need a cast that is not implicit. Then prefer exact matches, then preferred types of each type category, then the category of the `unknown` inputs. If more than one candidate is left, the error is `42725` with `operator is not unique`. If none is left, the error is `42883` with `operator does not exist` and the hint `No operator matches the given name and argument types. You might need to add explicit type casts.`

**Functions.** Function resolution is the same algorithm with named arguments, default arguments, `VARIADIC` and polymorphic types. At the pin, 231 rows of `pg_proc` have argument names, 37 have defaults, 38 are variadic and 248 use polymorphic types (`../2140/compat/postgres/12-functions-and-operators.md` section 12.5). If no function matches and the call has one argument and the name is a type, the call is a cast, so `int4('5')` works. When no candidate is left, PostgreSQL 19 gives a detail that names the reason: no function of that name, a function of that name outside the search path, the wrong number of arguments, the wrong argument names, misuse of `VARIADIC`, or the wrong argument types. The analyzer records the same reasons in the same order and selects the same text.

**Casts.** A cast has a context: implicit, assignment or explicit. The 249 rows of `pg_cast` give the context and the function of each built-in cast. A cast with no function is binary compatible. Casts between a type and its domain, between arrays, between composite types, and through the I/O functions of `text` follow `parse_coerce.c` (3,414 lines). An assignment of a value to a column with a typmod applies the length coercion of the type, so `'abcdef'::varchar(3)` gives `abc` and an `INSERT` of the same value into a `varchar(3)` column fails with `22001`.

**Collations.** Each expression of a collatable type has a collation and a derivation: implicit from a column, explicit from `COLLATE`, or none. Two different explicit collations conflict with `42P21`. Two different implicit collations give an indeterminate collation, and an operation that needs one fails with `42P22` and `could not determine which collation to use for string comparison`. The rules are in `parse_collate.c`.

The resolution happens once for each call site, when the statement is analyzed. Its result is one kernel with fixed argument types, so the executor does not resolve anything for each row.

## 7.7 Collations and locales

PostgreSQL has three collation providers: `libc`, which uses the operating system, `icu`, which uses the ICU library, and `builtin`, which PostgreSQL implements itself. `pg_collation.dat` at the pin has 7 rows: `default` (100), `C` (950), `POSIX` (951), `ucs_basic` (962), `unicode` (963), `pg_c_utf8` (811) and `pg_unicode_fast` (6411). `initdb` adds a row for each locale that the system and ICU have. The Unicode version of the pin is 17.0 (`src/include/common/unicode_version.h`).

**builtin.** rupg implements the builtin provider exactly. `C` and `POSIX` compare bytes. `C.UTF-8` compares code points and uses simple case mapping. `PG_UNICODE_FAST` compares code points and uses full case mapping, so `upper('ß')` is `SS`. The data comes from the Unicode 17.0 files that the pin vendors, and the build generates the tables with the scripts of the pin.

**icu.** rupg implements the ICU provider with ICU4X, the Rust implementation of the Unicode collation algorithm with CLDR data. The oracle uses ICU4C. Both use the root collation of CLDR and the same tailorings for the same CLDR version, so the order of strings should be the same. That is a prediction. `rupg-compat` tests it with the collation tests of the regression suite and with a corpus of sorted strings in every locale that `initdb` imports on the oracle host. A locale where the order differs is listed and published. The CLDR version of rupg must equal the CLDR version of the ICU that the oracle links, and `rupg-compat` records both.

**libc.** rupg cannot reproduce the collation of every C library. A database or column with provider `libc` and a locale other than `C` or `POSIX` uses the ICU collation of the same language and region, and `pg_collation` shows the provider and the locale that the user gave. This is a known difference for strings where glibc and CLDR disagree. Document 24 holds it as open question 7.B.

**The default for a new file.** A new `.rupg` file has the default collation `C.UTF-8` with provider `builtin`, and encoding `UTF8`. The oracle runs with `initdb --locale-provider=builtin --builtin-locale=C.UTF-8 --encoding=UTF8`, so the default comparison of the two servers is exact. `CREATE DATABASE ... LOCALE_PROVIDER icu ICU_LOCALE 'de-DE'` works as in PostgreSQL.

**Encodings.** The server encoding is `UTF8`. `SQL_ASCII` is also accepted, as PostgreSQL accepts it. `CREATE DATABASE` with another server encoding fails with `0A000`. The client side supports every client encoding that the pin supports, through the 98 conversions of `pg_conversion.dat`, which the build generates from the conversion tables of the pin.

## 7.8 Time zones

The time zone data is the tzdata release that the pin vendors in `src/timezone/data/tzdata.zi`, which is 2026e. The build compiles it into the binary, so a time zone answer does not depend on the host. `TimeZone` accepts every name that the pin accepts, POSIX zone strings, and offsets. `timezone_abbreviations` loads the abbreviation sets of the pin (`Default`, `Australia`, `India`), compiled in. `pg_timezone_names` and `pg_timezone_abbrevs` are virtual tables over the compiled data. When the pin moves to a new tzdata, the binary moves with it, as PostgreSQL minor releases do.

## 7.9 Types and their input and output

### 7.9.1 The surface

The pin has 197 rows in `pg_type`: 114 rows written in `pg_type.dat` and the array types that `genbki.pl` adds for them. Of the 114, 26 are pseudo-types, 6 are range types, 6 are multirange types and 4 are catalog row types, so 72 are base types that hold data. Each type has a text input function, a text output function, a binary receive function and a binary send function, unless it is a pseudo-type. Each one must give the same bytes as the pin, because `pg_regress` compares the text output byte for byte and drivers decode the binary output.

`rupg-types` holds one Rust type for each PostgreSQL storage form and the four functions of each type. It is at rank 0 and has no dependency on the engine. The executor uses the same functions on vectors, one call for each vector, with the type branch outside the loop.

### 7.9.2 The storage forms

The storage form is how a value is held in a vector and in the file. It need not be the PostgreSQL form, as long as the output is the same.

| Type | Storage form | Notes |
|---|---|---|
| `bool`, `int2`, `int4`, `int8`, `float4`, `float8`, `oid`, `oid8` | Native | |
| `numeric` with precision up to 18 | `i64` scaled by the column scale | Exact |
| `numeric` with precision 19 to 38 | `i128` scaled by the column scale | Exact |
| `numeric` without typmod, or with precision above 38 | Each value carries its own display scale. It is held as an `i128` with a scale when it fits, and otherwise in the PostgreSQL form with base 10,000 digits | The display scale is part of the value: `1.50` prints `1.50` |
| `date` | `i32` days since 1 January 2000 | The PostgreSQL epoch, with `infinity` and `-infinity` |
| `timestamp`, `timestamptz` | `i64` microseconds since 1 January 2000 | As PostgreSQL |
| `time`, `timetz` | `i64` microseconds, and for `timetz` an `i32` offset | |
| `interval` | months `i32`, days `i32`, microseconds `i64` | The three fields are kept apart, as PostgreSQL keeps them |
| `text`, `varchar`, `bpchar`, `name`, `bytea` | Length and bytes, with the encodings of document 09 in the cold store | `bpchar` keeps its padding in storage and drops it in comparison, as PostgreSQL does |
| `uuid` | 16 bytes | |
| `json` | The text as given | |
| `jsonb` | A binary form of our own with sorted keys | The output follows `jsonb.c`: keys sorted by length and then bytes, no duplicate keys |
| Arrays | Dimensions, lower bounds, a null bitmap and the elements | |
| Ranges and multiranges | Bounds and flags | |
| Enums | The OID of the label in storage, the sort order from `enumsortorder` | |
| Composites | One child vector for each attribute | |

A binary value that a client sends with `Bind` is checked and converted to the storage form by the receive function, so a value in storage is always valid.

### 7.9.3 Exact numeric results

`numeric` must give the same digits as PostgreSQL for every operation, including the scale of the result. The rules are in `numeric.c` (12,201 lines), and rupg ports them as rules with tests, not as a translation of the C code.

1. The precision limit is 1,000 digits in a typmod, and the scale is from -1,000 to 1,000 (`NUMERIC_MAX_PRECISION`, `NUMERIC_MIN_SCALE` and `NUMERIC_MAX_SCALE` in `numeric.h`).
2. Addition, subtraction and multiplication are exact. The result scale is the larger scale for addition and subtraction, and the sum of the scales for multiplication.
3. Division chooses its result scale with `select_div_scale`, which aims at at least 16 significant digits and at least the scale of either input. `sqrt`, `exp`, `ln`, `log` and `power` have their own scale rules in the same file.
4. Rounding to a typmod scale rounds half away from zero.
5. A cast from `float8` to `numeric` uses the shortest decimal string that reads back to the same `float8`.

The fast forms (`i64` and `i128`) give the same results as the general form. When an intermediate value does not fit, the kernel moves to the general form for that vector. A property test compares the three forms with the oracle on random inputs.

### 7.9.4 Text output rules that clients see

**Floats.** With `extra_float_digits` above 0, which is the default of 1, the output is the shortest string that reads back to the same bits. PostgreSQL uses Ryu for the digits and its own layout: fixed notation when the decimal exponent is from -4 to 14, else scientific with a sign and at least two exponent digits, as in `1e+15` and `1e-05`. rupg uses the digit generation of the `ryu` crate and ports the layout. With `extra_float_digits` at 0 or below, the output is `%.*g` with `DBL_DIG + extra_float_digits` digits. The special values are `NaN`, `Infinity` and `-Infinity`, and negative zero prints `-0`.

**Dates and times.** The output follows `DateStyle` and `IntervalStyle`, with all four date styles (`ISO`, `SQL`, `Postgres`, `German`), the field orders `DMY`, `MDY` and `YMD`, and all four interval styles. A `timestamptz` prints in the session time zone. Years before 1 print with `BC`. The rules are in `datetime.c` (5,425 lines) and `timestamp.c` (7,011 lines). The input accepts every form that the pin accepts, including the special strings `epoch`, `infinity`, `now`, `today`, `tomorrow`, `yesterday` and `allballs`.

**Formatting.** `to_char`, `to_date`, `to_number` and `to_timestamp` with a template follow `formatting.c` (7,002 lines), with the `lc_numeric`, `lc_monetary` and `lc_time` rules.

**bytea.** The output is `hex` or `escape` by `bytea_output`.

**Soft errors.** Every input function supports soft errors, so `pg_input_is_valid` and `pg_input_error_info` give the same answer and the same message as the pin. `COPY ... ON_ERROR` uses the same path.

### 7.9.5 Typmods

| Type | Typmod | Example |
|---|---|---|
| `varchar(n)`, `bpchar(n)` | n + 4 | `varchar(255)` is 259 |
| `numeric(p,s)` | ((p << 16) or (s and 0x7ff)) + 4 | `numeric(10,2)` is 655366 |
| `time(p)`, `timetz(p)`, `timestamp(p)`, `timestamptz(p)` | p | `timestamp(3)` is 3 |
| `interval` with fields and precision | (range mask << 16) or precision | `interval day to second(3)` |
| `bit(n)`, `varbit(n)` | n | `bit(8)` is 8 |

The 4 is `VARHDRSZ`, kept for historical reasons. `format_type(oid, typmod)` turns the pair back into text, such as `character varying(255)`.

## 7.10 The rewriter

The rewriter runs after the analyzer and before the planner. It applies these changes to the query tree, in the order of `rewriteHandler.c`.

1. **Views.** A view is a query stored as the `_RETURN` rule in `pg_rewrite`. A reference to a view becomes a subquery with the stored tree. A simple view is updatable: an `INSERT`, `UPDATE`, `DELETE` or `MERGE` on it becomes the same command on its base table, with `WITH CHECK OPTION` applied.
2. **Rules.** A `DO ALSO` or `DO INSTEAD` rule on a table adds or replaces commands. Document 16 section 16.6 specifies rules.
3. **Row security.** A table with row security enabled gets the predicates of its policies for the current role and command. A superuser and the owner bypass them unless `FORCE ROW LEVEL SECURITY` is set.
4. **Defaults.** Column defaults, identity columns and generated columns fill the target list of `INSERT` and `UPDATE`.

The stored tree of a view or a rule is in rupg's own form. PostgreSQL stores it as `pg_node_tree` text, which clients do not parse. `pg_rewrite.ev_action` shows a text that rupg generates, and document 05 section 5.3 excludes its bytes. The text of `pg_get_viewdef` and `pg_get_ruledef` is compared (section 7.17).

## 7.11 The catalog inside the file

### 7.11.1 What is stored

One `.rupg` file holds one PostgreSQL cluster: its databases, roles, tablespaces as names, settings, host rules and every object. `rupg-catalog` stores the objects as rows of system tables inside the file. The system tables are normal rupg tables in the hot store, with the same MVCC, undo and log as user tables (document 11). There are two groups. Shared tables hold roles, role memberships, databases, tablespace names, the database and role settings, the host rules, the `ALTER SYSTEM` settings and the shared comments and security labels. Per-database tables hold everything else.

`rupg-catalog` does not store the PostgreSQL catalogs. It stores one row for each object in rupg's own form: a relation with its columns, constraints, indexes and options; a type; a function with its body; and so on. The PostgreSQL catalogs are views of these rows that `rupg-pgcatalog` derives (section 7.13). Some PostgreSQL catalogs have no other form, and rupg stores them as they are: `pg_description`, `pg_shdescription`, `pg_seclabel`, `pg_shseclabel`, `pg_db_role_setting`, `pg_default_acl` and `pg_init_privs`.

### 7.11.2 Transactional DDL

DDL is transactional, as in PostgreSQL. `CREATE TABLE` inside a transaction is visible to that transaction at once and to other sessions after commit. A rollback removes it. Migration tools such as Flyway, Alembic and Rails depend on this: they run DDL in a transaction and read the catalog inside it to check their work. A catalog read is a read of the system tables at the snapshot of the statement, so this follows from MVCC with no special code.

### 7.11.3 The catalog version

The catalog has one version number. It is the commit timestamp of the most recent transaction that changed the catalog. The commit timestamp is the 64-bit hybrid logical clock value of document 04 section 4.7, so the version only grows, and two versions from two nodes of a cluster can be compared.

A session reads the catalog at the newest version at or below its snapshot, plus the uncommitted changes of its own transaction. The node keeps an immutable in-memory form of each catalog version that a live snapshot can see, built from the system tables and shared by every session. A new version shares every unchanged object with the previous one, so a small DDL costs a small amount of memory. This in-memory form is the shared catalog cache that document 06 section 6.4.2 counts outside the session.

The plan cache key has the catalog version. A plan also records the objects it depends on and the version at which each one last changed. When the version in the key is old, the cache checks the dependencies first. If none of them changed, the plan is still valid and its key moves to the new version. So a `CREATE TABLE` in one schema does not make every plan of the node plan again.

**Temporary objects.** A temporary table or view belongs to one session and is invisible to every other session. Its catalog rows live in an overlay of the session, not in the shared system tables, and its creation does not change the catalog version of the node. PL/pgSQL code that creates and drops a temporary table in a loop is common, and this keeps it from invalidating the plans of other sessions.

**In a cluster.** The system tables are one shard that every node replicates through its own Raft group (document 18). A node may run a statement at snapshot T only after it has applied every catalog change with a version at or below T. The version is a timestamp, not a counter on one node, so this rule needs no other coordination. Document 18 says how a DDL waits for the nodes.

## 7.12 OIDs

An OID is an unsigned 32-bit number. Built-in objects have the OIDs of the pin. Clients hard-code many of them: pgjdbc has the type OIDs as constants, Npgsql maps them, and every driver decodes a result column by its type OID. PostgreSQL reserves these ranges in `transam.h`.

| Range | Use |
|---|---|
| 1 to 9,999 | OIDs written in the `.dat` files and the headers |
| 10,000 to 11,999 | OIDs that `genbki.pl` assigns at build time |
| 12,000 to 16,383 | Objects that `initdb` creates with SQL: the system views, `information_schema`, the SQL functions and `plpgsql` |
| 16,384 and up | User objects |

**Built-in OIDs.** The build gets the first two ranges from the vendored `.dat` files with the rules of `genbki.pl`. The third range is not in any file. `rupg-compat` dumps the oracle's rows below 16,384 for these objects into a fixture, and rupg vendors the fixture beside the `.dat` files. When the pin moves, the fixture moves in the same change.

**User OIDs.** A counter in the shared system tables gives user OIDs from 16,384. It is durable, so a restart does not reuse an OID. When it passes 4,294,967,295 it goes back to 16,384, and before it uses an OID it checks that no object of the same kind has it, as `GetNewOidWithIndex` does. In a cluster, a node takes a block of OIDs from the catalog shard and gives them out locally, so OIDs are unique across the cluster.

**The set of OIDs of one command.** One command can make more than one object, and a client that caches OIDs sees all of them. rupg makes the same objects in the same order as PostgreSQL. `CREATE TABLE t (a int)` makes the relation, its row type and the array of the row type. A `PRIMARY KEY` makes an index and a constraint. A `NOT NULL` makes a constraint row, from PostgreSQL 18. A `serial` column makes a sequence with its row type and array type. `CREATE VIEW` makes a relation, a row type, an array type and a `pg_rewrite` row. `CREATE TYPE ... AS ENUM` makes the type, the array type and one OID for each label. rupg has no TOAST tables, so it does not make the three objects of a TOAST relation. `rupg-compat` compares the differences between consecutive user OIDs on both servers and skips the TOAST objects (document 05 section 5.3, item 11).

## 7.13 The virtual pg_catalog

### 7.13.1 Virtual tables

The 64 catalogs of the pin, 11 of them shared, are virtual tables in `rupg-pgcatalog`. A virtual table has a fixed schema and a provider. When a query scans it, the provider reads the catalog version of the statement and produces column vectors. There is no second copy of the catalog, so the two cannot drift apart.

Each virtual table follows these rules.

1. Declare every column of the PostgreSQL catalog with the same name, the same type OID and the same order. `SELECT *` returns that order, and some clients read columns by position.
2. Generate the declaration from the `CATALOG` struct in the vendored header at build time. Do not write it by hand.
3. Fill a column that rupg does not track with the value that PostgreSQL has for a new object, for example `relpages` 0 and `reltuples` -1.
4. Expose `oid` as a normal column.
5. Expose the system columns of section 7.16.
6. Accept pushed-down predicates on `oid`, on the name column and on the columns that clients join on, such as `attrelid` and `relnamespace`.

A catalog that rupg has no use for, such as `pg_ts_parser` before text search exists, still exists with every column and its built-in rows from the data files.

### 7.13.2 Static rows

The build parses the vendored `.dat` files with the rules of `genbki.pl`: it expands the array types, fills each default from `BKI_DEFAULT`, and turns symbolic references into OIDs, so `typinput => 'int4in'` becomes the OID of that `pg_proc` row. It writes the rows as static column batches into the binary. The batches cover `pg_type`, `pg_proc`, `pg_operator`, `pg_cast`, `pg_am`, `pg_amop`, `pg_amproc`, `pg_opclass`, `pg_opfamily`, `pg_namespace`, `pg_language`, `pg_collation`, `pg_authid`, `pg_database`, `pg_tablespace`, `pg_aggregate`, `pg_range`, `pg_conversion` and the text search catalogs. The `descr` fields become rows of `pg_description`. The build also generates the `pg_class` and `pg_attribute` rows of the 64 catalogs from their headers. A test compares every generated row with a dump of the oracle and fails on the first difference.

For a shim version, the same code reads the vendored files of that version and the build writes the differences (document 05 section 5.6.4).

### 7.13.3 The rows that exist before the first user object

| Catalog | Rows |
|---|---|
| `pg_namespace` | `pg_catalog` 11, `pg_toast` 99, `public` 2200, `information_schema` from the fixture |
| `pg_database` | `template1` 1, `template0` 4, `postgres` 5 |
| `pg_authid` | The predefined roles of `pg_authid.dat`, with the bootstrap superuser at OID 10 |
| `pg_tablespace` | `pg_default` 1663, `pg_global` 1664 |
| `pg_am` | `heap` 2, `btree` 403, `hash` 405, `gist` 783, `gin` 2742, `spgist` 4000, `brin` 3580 |
| `pg_language` | `internal` 12, `c` 13, `sql` 14, `plpgsql` from the fixture |

The bootstrap superuser has OID 10. Its name is the creation option `superuser`, as `initdb -U` gives it, with the default `postgres`. `heap` is in `pg_am` because clients expect it and because `relam` of a table points to it. A rupg table shows `relam` 2 unless it was created with `USING columnar`, which document 05 section 5.10 describes.

`template0` and `template1` exist as names. `CREATE DATABASE` copies the catalog of its template, which in rupg is a copy of the per-database system table rows, not of files.

### 7.13.4 Cost

Catalog queries are on the connect path. The target is that a catalog query on rupg is never slower than the same query on the oracle over the same transport. The static rows are batches that already exist, so a scan of the built-in part of `pg_type` allocates nothing for each row. Each catalog has an index by `oid` and by name, and the provider uses it for an equality predicate. A provider that gets `attrelid = 16384` reads one relation and nothing else. The user part of each virtual table is cached for each catalog version and rebuilt only after DDL. Two queries set the targets in `../2140/compat/postgres/07-the-catalog.md` section 7.15: the Npgsql type load must finish in under 2 ms on a database with 100 user types, and the DBeaver schema tree must load in under 1 second on a database with 10,000 tables.

## 7.14 information_schema and the system views

The pin defines 86 system views in `system_views.sql` (1,541 lines), and 65 views, 11 functions and 5 domains in `information_schema.sql` (3,011 lines). `system_functions.sql` (368 lines) defines 45 functions in SQL. rupg does not rewrite these by hand. When a file is created, rupg runs the three vendored scripts through its own SQL path, as `initdb` does, with the OIDs of the fixture. So the views are real views over the virtual catalogs, with the bodies of the pin, and their results follow from the catalogs. `pg_get_viewdef` of each one must give the text of the oracle, which tests the deparser of section 7.17.

The views run in rupg's executor. A view such as `information_schema.columns` joins many catalogs. The predicate pushdown of section 7.13.1 must reach through the view body to the providers, so that a query for one table reads the rows of one table. `rupg-bench` measures the catalog queries that the recording proxy captured, against the oracle.

## 7.15 Statistics views

Of the 86 system views, 50 are `pg_stat*` and `pg_statio*` views. They are defined in `system_views.sql` over functions such as `pg_stat_get_numscans(oid)`. rupg implements these functions from its own counters. The values are not compared (document 05 section 5.3, item 3), but the columns, the types and the presence of a row for each object are compared.

| View group | Source in rupg |
|---|---|
| `pg_stat_activity`, `pg_stat_ssl`, `pg_stat_gssapi` | The session table of document 06 section 6.17 |
| `pg_stat_*_tables`, `pg_stat_*_indexes`, `pg_statio_*` | Counters for each table and index, kept on each worker and summed when read |
| `pg_stat_database`, `pg_stat_database_conflicts` | Counters for each database |
| `pg_stat_io`, `pg_stat_wal`, `pg_stat_checkpointer`, `pg_stat_bgwriter` | The buffer manager, the log rings and the checkpoint of documents 08 and 11, mapped to the nearest PostgreSQL column. A column with no meaning in rupg is 0 |
| `pg_stat_progress_*` | The progress of `CREATE INDEX`, `VACUUM`, `ANALYZE`, `COPY` and `REPACK`, where rupg has the operation |
| `pg_stat_replication`, `pg_stat_subscription`, `pg_stat_replication_slots` | Logical replication from M9 |
| `pg_stat_user_functions` | PL/pgSQL and SQL function calls when `track_functions` is set |
| `pg_stats`, `pg_stats_ext`, `pg_stats_ext_exprs` | The statistics of document 15, in the PostgreSQL form: `null_frac`, `n_distinct`, `most_common_vals`, `histogram_bounds`, `correlation` and the rest |

`pg_stat_reset()` and its siblings reset the rupg counters. `stats_reset` columns, new in 19, show the time of the last reset.

## 7.16 System columns and the reg types

**System columns.** Every table has the system columns `tableoid`, `ctid`, `xmin`, `xmax`, `cmin` and `cmax`. A query that names them gets values. `tableoid` is the OID of the table. `ctid` is a `tid` that rupg computes from the row id, and a query with `WHERE ctid = $1` finds the row. Document 10 owns the mapping from row id to `ctid`. `xmin` and `xmax` are the 32-bit transaction ids of document 11 section 11.9. Entity Framework uses `xmin` as a concurrency token, so `xmin` must change when the row changes and must stay the same otherwise. DataGrip reads `xmin` from `pg_class` and calls `age(xmin)`. The values are excluded from the comparison, and the rules are not.

**The reg types.** `regclass`, `regtype`, `regproc`, `regprocedure`, `regoper`, `regoperator`, `regnamespace`, `regrole`, `regconfig`, `regdictionary`, `regcollation` and, new in 19, `regdatabase` are OIDs with a name lookup in their input and output. `'t'::regclass` looks up `t` in the search path and gives its OID. The output of a `regclass` value is the name, qualified only when the object is not visible in the search path, and quoted when needed. `to_regclass` and its siblings return NULL in place of an error. Every driver and GUI uses these casts in its catalog queries, so they are part of the tier 1 catalog of document 05 section 5.1.

## 7.17 Deparse

Clients turn catalog rows into text with deparse functions: `pg_get_viewdef`, `pg_get_ruledef`, `pg_get_indexdef`, `pg_get_constraintdef`, `pg_get_triggerdef`, `pg_get_functiondef`, `pg_get_function_arguments`, `pg_get_function_result`, `pg_get_expr`, `pg_get_partkeydef`, `pg_get_statisticsobjdef`, `pg_get_serial_sequence` and `format_type`. pg_dump, psql's `\d`, every GUI and every ORM schema dumper call them. The text is compared byte for byte.

PostgreSQL implements them in `ruleutils.c`, which has 13,804 lines. rupg ports the rules into a deparser in `rupg-pgcatalog` that works on rupg's stored trees. The rules include where parentheses go, which casts are shown, how names are quoted and qualified for the current search path, how a `CASE` and a window clause are laid out, and the pretty-print flag. The test runs each deparse function on every object that the regression suite creates, and on the views of section 7.14, and compares the text with the oracle.

## 7.18 Functions and operators, and how they are tested

### 7.18.1 The size

The pin has 3,414 functions, of which 163 are aggregates, 15 are window functions and 116 return sets, 805 operators and 249 casts. 49 of the functions are written in SQL in `pg_proc.dat`. rupg implements every function whose `prolang` is `internal` as a Rust kernel in `rupg-func`, and runs the SQL functions through its own SQL path, as PostgreSQL does.

### 7.18.2 Generated signatures

Nobody writes a function signature by hand. The build reads `pg_proc.dat` and generates a table with the OID, the name, the argument types, the result type, the volatility, the strictness, the parallel safety, the cost, the row estimate and the `prosrc` name of every function. A kernel registers itself under its `prosrc` name, for example `int4pl`. The build fails if a `prosrc` name has no kernel and is not in the list of functions that are not done yet. That list is published with the compatibility number, so it can only shrink by work.

A kernel works on vectors. A strict function gets the null mask and does not see the NULL rows. A function marked `IMMUTABLE` is folded at plan time when its arguments are constants.

### 7.18.3 The order of work

The work is done one family at a time. A family is a set of functions that share one parser, one engine or one type. A family is closed in one change: every function, every overload, the exact error texts and the regression tests that cover it. The order is that of `../2140/compat/postgres/12-functions-and-operators.md` section 12.9, moved onto the rupg milestones.

| Milestone | Families |
|---|---|
| M2 | Session information: `version`, `current_database`, `current_schema`, `current_user`, `pg_backend_pid`, `current_setting`, `set_config`. Catalog information: `format_type`, the `pg_*_is_visible` functions, `has_*_privilege`, `obj_description`, `col_description`, the `reg*` casts |
| M3 | Core scalar functions, date and time, formatting, aggregates and windows, arrays, JSON and `jsonb`, SQL/JSON path, pattern matching, sequences, soft errors, the deparse functions, `uuidv7` and `uuidv4` |
| M5 | Advisory locks, `pg_cancel_backend`, `pg_terminate_backend`, `txid_*` and `pg_current_xact_id` |
| M6 | Enums, ranges and multiranges with their index support, network types |
| M7 | Event trigger functions, the functions that the PL/pgSQL tests need |
| M12 | Text search, geometry, XML |

### 7.18.4 Tests

A function is tested in four ways. The regression suite calls it as part of L3. The differential corpus calls every function with generated arguments, including NULLs, empty values, the limits of each type and values that fail, and compares the result or the error with the oracle. A property test checks the vector kernel against the scalar form of the same function on random vectors with random null masks. A benchmark per family runs in `rupg-bench`, so a change that slows a kernel is seen.

## 7.19 Gates

| Milestone | What this document must deliver |
|---|---|
| M2, gate G1 | The tier 1 catalog: `pg_type`, `pg_namespace`, `pg_class`, `pg_attribute`, `version()`, `format_type` and the `reg*` casts, at 100 percent against the oracle. The type I/O of every type that the connect matrix reads |
| M3, gate G4 | The vendored grammar with no conflict and every production with an action. The analyzer, the rewriter for views, the tier 2 catalog, `information_schema` and the system views, transactional DDL, the catalog version and the plan cache rule, the families of M3, the deparse functions |
| M5, gate G5 | The families of M5 |
| M6 | The families of M6 and the operator classes of every index type in `pg_opclass` |
| M7, gate G6 | Rules and the rest of the rewriter. The regression suite at 100 percent of its denominator |
| M9 | The shim catalogs of 14 to 18 at the level of 19 |
| M12, gate G12 | Text search, geometry and XML. L2 and L3 at 100 percent |

## 7.20 Open questions from this document

7.A. Is ICU4X's collation order equal to the ICU4C order of the oracle for every locale that `initdb` imports? Section 7.7 predicts yes for the same CLDR version. The corpus test decides.

7.B. How should rupg handle `libc` collations other than `C` and `POSIX`? Section 7.7 maps them to ICU. The alternative is to read the collation data of the C library on the host, which is not possible on every platform and not in WebAssembly.

7.C. Should rupg support server encodings other than `UTF8` and `SQL_ASCII`? Section 7.7 refuses them. A count of users who need them, and of regression tests that depend on them, decides.

7.D. Is the dependency check of section 7.11.3 cheap enough to run on every plan cache hit after DDL? A workload with frequent DDL, such as a migration run beside TPC-C, decides.
