# Changelog

All changes that a user can see are in this file. The version is 0.M.patch, where M is the last milestone that closed. See [`spec/23-milestones.md`](spec/23-milestones.md) section 23.17.

## Unreleased

### Added

- `pg_stat_get_autovacuum_scores` runs, so the view `pg_stat_autovacuum_scores` gives one row for each table, materialized view and TOAST table that is not temporary. The scores and the flags `do_vacuum`, `do_analyze` and `for_wraparound` come from the same steps as `relation_needs_vacanalyze`: the age of `relfrozenxid` and `relminmxid`, the counts of dead, inserted and changed rows, the settings of autovacuum and the options of the table. A TOAST table without its own options uses the options of its main table (#678).
- `backend_xid` and `backend_xmin` of `pg_stat_activity` show the ID of the transaction of each session and the `xmin` of its snapshot. In `READ COMMITTED`, the `xmin` goes at the end of each statement, and in `REPEATABLE READ` it stays until the end of the transaction, as `PGPROC->xmin` does (#680).
- `UPDATE ... SET (a, b) = (SELECT ...)` runs, also in `ON CONFLICT DO UPDATE` and `MERGE`. The subquery runs once for each row, no row gives nulls, and a second row is an error. `EXPLAIN` shows the subplan as `multiexpr_1`. A subquery that gives one value now stops at its second row, as in PostgreSQL, so a volatile function in it runs no more often than in PostgreSQL (#684).
- `MERGE` into an automatically updatable view writes the base table of the view, as an `UPDATE`, an `INSERT` or a `DELETE` of the view does. The conditions of the view limit the rows of the target, so a row that the view does not show is not matched. `WITH CHECK OPTION` checks the new rows of the inserts and the updates, and the defaults of the view apply. The errors of a view that is not updatable, of a column that is not updatable and of a view with `INSTEAD OF` triggers for some actions but not all are those of PostgreSQL (#686).
- `MERGE` into a view with `INSTEAD OF` triggers for all its actions calls the trigger of the action of each row, as an `INSERT`, an `UPDATE` or a `DELETE` of the view does. A trigger that gives a null row skips the action, and `RETURNING` shows the row that the trigger gave (#687).
- `MERGE` with only `DO NOTHING` actions now runs on any view, also on a view that is not automatically updatable. It reads the view and fires no triggers, as PostgreSQL does (#689).
- `INSERT ... ON CONFLICT` into an automatically updatable view. The inference, `SET` and `EXCLUDED` name the columns of the view, and a column of the view that is an expression infers an index on that expression. `ON CONSTRAINT` looks for a constraint of the view, as in PostgreSQL. `WITH CHECK OPTION` checks the row that `DO UPDATE` writes (#690).
- `INSERT ... ON CONFLICT DO SELECT`, with `WHERE` and the locks `FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR SHARE` and `FOR KEY SHARE`. A row with a conflict gives the row that the table has to `RETURNING`, and the statement writes no row and fires no trigger `UPDATE`. `RETURNING` is required, a lock needs the privilege `UPDATE`, and a lock of a row that the same command inserted is an error. `EXPLAIN` shows the resolution with its lock (#NNN).

### Fixed

- A function in PL/pgSQL in the expression or the predicate of a unique index can read and change the tables. Before, such a function made all the sessions wait with no end, as the write held the lock of the rows while the function waited for the same lock. The expressions of an index now run before the write takes the lock, and each index keeps the keys of its rows. An exception block in such a function no longer rolls back the rows that `CREATE INDEX` or the write wrote before the function ran (#685).
- The `xmin` of `pg_current_snapshot` and `txid_current_snapshot` is the transaction of the session when it is the oldest transaction in progress, as `GetSnapshotData` gives it. Before, the session did not use its own transaction for `xmin` (#680).

## 0.1.78 (2026-10-25)

### Added

- A function in SQL can have more than one statement, and its last statement can be `INSERT`, `UPDATE`, `DELETE` or `MERGE` with `RETURNING`. The statements run one after the other, and each one sees the changes of the statements before it. A statement can also be a utility statement, such as `CREATE TABLE`. `CREATE FUNCTION` checks each statement, and with `check_function_bodies` set to off it checks no statement. An error at run time has the line `SQL function "name" statement N`. A function that is not volatile cannot write, and no function can run `COMMIT` or `ROLLBACK` (#676).
- `CREATE TABLESPACE` and `DROP TABLESPACE` run for an in-place tablespace of `allow_in_place_tablespaces`, with the checks of PostgreSQL: the privilege, the location, the name, the options and the owner. The new rows are in `pg_tablespace`, and the tablespace functions, the privilege functions and `pg_describe_object` find them. A role that owns a tablespace cannot be dropped (#677).
- `REINDEX` runs for an index, a table, a partitioned table or index, a schema, the system catalogs and the database, with the options `VERBOSE`, `CONCURRENTLY` and `TABLESPACE pg_default`. The checks are those of PostgreSQL: the kind of the relation, the privilege `MAINTAIN`, the owner of the schema or the database, the transaction block and the catalogs with `CONCURRENTLY`. Each index that the statement builds again gets a new `relfilenode`, and `VERBOSE` gives the messages of PostgreSQL. A temporary table with a primary key or a unique constraint can now be the first temporary object of a session (#679).
- `CREATE INDEX CONCURRENTLY` runs. It cannot run in a transaction block, in a function, in a procedure or in `CREATE SCHEMA`, and it cannot run on a partitioned table. When the build fails, the index stays with `indisvalid` and `indisready` false, as the first transaction of `DefineIndex` commits it. An index that is not ready checks no row. `REINDEX` builds such an index again and makes it valid and ready, and `REINDEX TABLE CONCURRENTLY` skips it with the warning of PostgreSQL. `pg_index.indisready` now shows the state of the index (#682).

## 0.1.77 (2026-10-24)

### Added

- `pg_current_xact_id`, `pg_current_xact_id_if_assigned`, `pg_current_snapshot` and `pg_xact_status` run, and so do `txid_current`, `txid_current_if_assigned`, `txid_current_snapshot` and `txid_status`. A transaction gets its ID at the first write, at a change of the catalog, at `LOCK ... IN ACCESS EXCLUSIVE MODE` or at a call of `pg_current_xact_id`. The snapshot has `latestCompletedXid + 1` as `xmax` and does not show the transaction of the session, as `GetSnapshotData` makes it. `pg_xact_status` gives `committed`, `aborted` or `in progress`, and `22023` for an ID in the future (#667).
- The system columns `xmin` and `xmax` give the transaction that inserted the row and the transaction that deleted it, also for the rows of a partitioned table and of a table with children of `INHERITS`. A row that an aborted transaction deleted keeps the ID of that transaction in `xmax`, as `t_xmax` keeps it. `RETURNING xmin` of an `INSERT` or an `UPDATE` gives the transaction of the session. Before, each row showed a frozen `xmin` and 0 as `xmax` (#668).
- `age(xid)` and `mxid_age(xid)` run. `age` counts from the ID of the transaction, or from the next ID when the transaction has none, and the value stays the same until the end of the transaction, as `GetStableLatestTransactionId` gives it. An ID that is not normal gives `2147483647` (#669).
- The clause `SET` of `CREATE FUNCTION`, `CREATE PROCEDURE` and `ALTER FUNCTION`, with `RESET`, `RESET ALL` and `SET ... FROM CURRENT`. `proconfig` keeps each setting, and `pg_get_functiondef` shows each one on its own line. A call runs with the settings, and they go back at the end of the call, also after an error. A value that a `SET` in the call changes stays after the call, as `AtEOXact_GUC` keeps it. A body in a string reads its names with the `search_path` of the settings. `SECURITY INVOKER` is accepted (#671).
- `SECURITY DEFINER` for `CREATE FUNCTION`, `CREATE PROCEDURE` and `ALTER FUNCTION`. A call runs as the owner of the function: the checks of the privileges, `current_user`, `"$user"` in `search_path` and the owner of a new object use the owner. `SET ROLE`, `SET SESSION AUTHORIZATION` and `set_config` of them give an error in the call, as in PostgreSQL. `prosecdef`, `pg_get_functiondef` and `information_schema.routines` show the option (#674).

### Fixed

- On macOS and on Linux on arm64, `pg_settings` and `SHOW` give the boot values and the options of PostgreSQL on that host. On macOS, `bgwriter_flush_after` and `checkpoint_flush_after` are 0, `file_extend_method` is `write_zeros`, `wal_sync_method` is `open_datasync` and has the option `fsync_writethrough`, and `recovery_init_sync_method` does not have `syncfs`. On arm64, `timing_clock_source` does not have `tsc` (#670).

- On macOS, the server sets `SO_KEEPALIVE` on each TCP connection, and `tcp_keepalives_idle`, `tcp_keepalives_interval` and `tcp_keepalives_count` read and set the options of the socket, as on Linux. Before, `SHOW` gave 0 for them on macOS. macOS does not have `TCP_USER_TIMEOUT`, so `tcp_user_timeout` shows 0, and a value that is not 0 writes `setsockopt(TCP_USER_TIMEOUT) not supported` to the log, as in PostgreSQL (#672).

## 0.1.76 (2026-10-24)

### Added

- `DROP SCHEMA public` removes the schema `public`, which `initdb` makes but does not pin. `CASCADE` removes the objects in it, and a later `CREATE SCHEMA public` makes a new schema. The search path, `regnamespace` and the scans of `pg_namespace`, `pg_description` and `pg_init_privs` do not find the removed schema (#653).
- An `INSERT`, an `UPDATE` or a `DELETE` of an automatically updatable view writes the base relation of the view, as `rewriteTargetView` does. The view must read one table or view, and it must not have `DISTINCT`, `GROUP BY`, `HAVING`, a set operation, `WITH`, `LIMIT`, `OFFSET`, aggregates, window functions, set-returning functions or `TABLESAMPLE`. The conditions of the view apply to an `UPDATE` and a `DELETE`, also for `security_barrier` and for a view on a view. `SET` and the columns of an `INSERT` name the columns of the view, a column that is not updatable gives the error of PostgreSQL, and the defaults of the columns of the view apply. `RETURNING` gives the columns of the view. The statement needs the privileges on the view, and the owner of the view needs the privileges on the base relation (#650).
- `WITH CHECK OPTION`, `WITH LOCAL CHECK OPTION` and `WITH CASCADED CHECK OPTION` of `CREATE VIEW`, and the option `check_option` of `CREATE VIEW` and `ALTER VIEW`. An `INSERT` or an `UPDATE` of the view checks each row that it writes against the condition of the view, and a `CASCADED` check also checks the conditions of the views under it. The check comes after the write and before `RETURNING`, the inner view first, as `ExecWithCheckOptions` does it, and a row that fails gives `44000` with the row in the detail. A view that is not automatically updatable cannot have the option (#651).
- `COPY ... TO STDOUT` with `FORMAT json` sends each row as a JSON object, with the names of the columns as the keys, as `CopyToJsonOneRow` does. `FORCE_ARRAY` puts the rows in a JSON array. A column list, a query, a partitioned table and all the types work, and `CopyOutResponse` gives one text column (#652).
- `CREATE STATISTICS`, `ALTER STATISTICS ... SET STATISTICS`, `DROP STATISTICS`, `COMMENT ON STATISTICS`, and `RENAME TO`, `OWNER TO` and `SET SCHEMA` of a statistics object. The definition has the checks of `CreateStatistics`, the objects are in `pg_statistic_ext` with their rows of `pg_depend`, and a name without a schema gets the name that `ChooseExtendedStatisticName` makes. `pg_get_statisticsobjdef`, `pg_get_statisticsobjdef_columns`, `pg_get_statisticsobjdef_expressions`, `pg_statistics_obj_is_visible`, `pg_describe_object` and `pg_identify_object` give the objects. rupg does not collect the statistics yet (#654).
- Virtual generated columns: `GENERATED ALWAYS AS (expr) VIRTUAL` in `CREATE TABLE`, with the checks of `CheckAttributeType` and of the functions and types that the expression can use. The table stores a null for the column, and each read of the column gets the expression in its place, as `expand_virtual_generated_columns` does. A read on the nullable side of an outer join gets a null when the join has no row there. Check constraints and not-null constraints test the value of the expression, and the detail of the error shows `virtual` for the column. An `INSERT` through a view with a default for the column fails as in PostgreSQL (#657).
- `CREATE FUNCTION` in the languages `internal` and `c`, which only a superuser can use. A function in `internal` runs the built-in function that its `AS` names, and the name must be a built-in function, as `fmgr_internal_validator` checks. A function in C can link to the functions of `regress.so`, the library of the regression tests of PostgreSQL, by its path, and rupg runs `binary_coercible`, `is_catalog_text_unique_index_oid`, `interpt_pp`, `reverse_name`, `test_canonicalize_path`, `test_atomic_ops` and some other functions of that library. Another library or a function that the library does not have gives the error of `fmgr_c_validator`. `LOAD` of that library works. The default `procost` of a function in these languages is 1 (#625).
- `pg_indexam_has_property`, `pg_index_has_property` and `pg_index_column_has_property` give the properties of the six built-in index access methods, of an index and of an index column, as `amutils.c` does. The answers for `gist` and `spgist` use the support functions and the operators of the operator class. Before, the three functions gave `0A000`. Entity Framework Core calls `pg_indexam_has_property` when it reads the indexes of a database.
- A statement checks the privileges that it needs on the tables and the columns that it reads and writes before it runs, as `ExecCheckPermissions` does. A column grant is enough for the columns that the statement uses, the relations of a view count as the owner of the view unless the view has `security_invoker`, and `FOR UPDATE` needs `UPDATE`. (#619)
- `CREATE FUNCTION` in the languages `internal` and `c`, which only a superuser can use. A function in `internal` runs the built-in function that its `AS` names, and the name must be a built-in function, as `fmgr_internal_validator` checks. A function in C can link to the functions of `regress.so`, the library of the regression tests of PostgreSQL, by its path, and rupg runs `binary_coercible`, `is_catalog_text_unique_index_oid`, `interpt_pp`, `reverse_name`, `test_canonicalize_path`, `test_atomic_ops` and some other functions of that library. Another library or a function that the library does not have gives the error of `fmgr_c_validator`. `LOAD` of that library works. The default `procost` of a function in these languages is 1 (#625).
- A statement checks the privileges that it needs on the tables and the columns that it reads and writes before it runs, as `ExecCheckPermissions` does. A column grant is enough for the columns that the statement uses, the relations of a view count as the owner of the view unless the view has `security_invoker`, and `FOR UPDATE` needs `UPDATE`. (#619)
- `LOCK TABLE` in all eight modes. The statement takes the lock on each relation, on the children of a table without `ONLY`, and on the tables that a view reads, and the transaction holds the locks until it ends. A lock that conflicts with a lock of another session waits with the wait event `relation`, or gives `55P03` "could not obtain lock on relation" with `NOWAIT`. `lock_timeout`, `deadlock_timeout` and the deadlock check work as for the advisory locks. Only `LOCK TABLE` takes these locks for now, so the other statements do not wait for them. Before, the modes stronger than `ROW EXCLUSIVE` gave `0A000`. Ecto and Entity Framework Core use these modes in their migrations.
- `CREATE TABLE ... (LIKE source INCLUDING STATISTICS)`, and `INCLUDING ALL`, copy the statistics objects of the source to the new table, as `generateClonedExtStatsStmt` does. A copy has the kinds and the expressions of the object, with the column numbers of the new table, and a name that `ChooseExtendedStatisticName` gives. With `INCLUDING COMMENTS`, it also has the comment of the object (#662).
- `DROP OWNED BY` and `REASSIGN OWNED BY`, as `shdepDropOwned` and `shdepReassignOwned` run them. `DROP OWNED` revokes the privileges of each role on the objects that other roles own, drops the objects that the roles own with `RESTRICT` or `CASCADE`, and removes the grants of membership that the roles made. `REASSIGN OWNED` gives the objects of the roles to the new role, and a table takes its indexes, its sequences and its row type with it. Both statements check that the role of the statement has the privileges of each role, and they refuse the roles of `initdb`. `ALTER FUNCTION ... OWNER TO` and `ALTER TYPE ... OWNER TO` now also change the old owner to the new owner in the ACL of the object, as `aclnewowner` does (#663).
- `ALTER DEFAULT PRIVILEGES` runs. It keeps the rows of `pg_default_acl`, and new tables, views, sequences, types, functions and schemas get the ACL of the default privileges of their owner. `DROP OWNED` revokes the default privileges and drops the ones of the role, `DROP ROLE` names them in its details, and `pg_describe_object` shows them (#665).
- `CREATE SCHEMA` runs its schema elements: `CREATE TABLE`, `CREATE VIEW`, `CREATE INDEX`, `CREATE SEQUENCE`, `CREATE TRIGGER` and `GRANT`. The elements run as the owner of the schema, with the new schema first in `search_path`, in the order of `transformCreateSchemaStmtElements`: sequences, tables, views, indexes, triggers and grants. A foreign key of a table becomes an `ALTER TABLE ... ADD CONSTRAINT` after the other elements, so it can refer to a later table. An element that names another schema or makes a temporary table gives the error of `checkSchemaNameRV`. Before, the statement gave `0A000` (#666).

### Fixed

- `pg_get_functiondef` now shows the body of a function in SQL with a `RETURN` body or a `BEGIN ATOMIC` body, as `print_function_sqlbody` does, in place of `AS` and a string. Before, it gave `0A000`. `CREATE PROCEDURE` with the empty body `BEGIN ATOMIC END` now gives `0A000` in place of `XX000` (#659).
- `pg_init_privs` has no row for a built-in object that `DROP` removed. Before, the row of the schema `public` stayed after `DROP SCHEMA public`, and `pg_dump` read it (#660).
- A function with a body in SQL depends on the tables and the functions that the subqueries of its `RETURN` expression read. Before, `DROP TABLE` did not find the function. The dependencies come in the order of `eliminate_duplicate_dependencies`, and a column takes the place of its whole relation, so a body in `BEGIN ATOMIC` does not also depend on the whole table (#656).
- The input of `regclass` in a default, a generation expression or a check constraint of `CREATE TABLE` finds the new table, as PostgreSQL makes the table before it reads the expressions (#658).
- A trigger `BEFORE ... FOR EACH ROW` of a table with stored generated columns now sees nulls in the generated columns of `NEW`, and the write computes the generated columns after the triggers. The condition `WHEN` of a trigger `BEFORE` that reads a generated column of `NEW`, or `NEW` as a whole row of a table with generated columns, gives the error of `CreateTriggerFiringOn`. A trigger `UPDATE OF` a generated column fires when the statement changes a column that the generation expression reads (#649).

## 0.1.75 (2026-10-24)

### Added

- The rules `ON INSERT`, `ON UPDATE` and `ON DELETE` of a table and of a view run, as `RewriteQuery` applies them. An action `INSERT`, `UPDATE` or `DELETE` of a rule sees the rows of the statement as `OLD` and `NEW`, with the new values of the stored generated columns, and the condition of the rule applies to each row. A rule `DO INSTEAD` replaces the statement, and a rule `DO INSTEAD` with a condition removes the rows for which the condition is true. The command tag comes from the statement or from its last action `DO INSTEAD` of the same kind. `RETURNING`, `ON CONFLICT`, `MERGE` and a rule that fires itself again give the errors of PostgreSQL (#647).
- `CREATE TABLE ... (LIKE source)` with the options `INCLUDING COMMENTS`, `COMPRESSION`, `CONSTRAINTS`, `DEFAULTS`, `GENERATED`, `IDENTITY`, `INDEXES`, `STORAGE` and `ALL`, and `EXCLUDING`, with the checks of `transformTableLikeClause` and `expandTableLikeClause`. The source can be a table, a view, a materialized view, a composite type or a partitioned table. The not-null constraints of the source are always copied. A check constraint or an index with a whole-row reference gives the error of PostgreSQL. A storage conflict with a parent of `INHERITS` now names the storage of the type (#646).
- Stored generated columns: `GENERATED ALWAYS AS (expr) STORED` in `CREATE TABLE`, with the checks of `transformColumnDefinition` and `cookDefault`. An `INSERT`, an `UPDATE`, a `MERGE`, `ON CONFLICT DO UPDATE` and `COPY FROM` compute the column before the check constraints, and a value that is not `DEFAULT` is an error. A leaf partition and a child of `INHERITS` compute their own columns. `COPY` skips a generated column. `pg_attribute.attgenerated` and the generated columns of `information_schema.columns` show the column (#641).
- The rules of `MergeAttributes` and `ALTER TABLE ... INHERIT` for generated columns: a child column is generated if and only if its parent column is, a child can give its own expression, and two parents must agree. `COPY FROM ... WHERE` cannot read a generated column (#642).
- `pg_get_function_sqlbody`, with the text of `print_function_sqlbody`. A `RETURN` body shows on one line, and a `BEGIN ATOMIC` body shows each query on its lines. A parameter shows the name of its argument, after the name of the function when the query has relations. A function with a body in a string gives null (#640).
- `ALTER AGGREGATE` with `RENAME TO`, `OWNER TO` and `SET SCHEMA`, with the checks of `AlterObjectRename_internal`, `AlterObjectOwner_internal` and `AlterObjectNamespace_internal`. A support function of an aggregate can get a new name (#633).
- `CREATE AGGREGATE` and `CREATE OR REPLACE AGGREGATE`, in the old and the new syntax, with the checks of PostgreSQL, the rows of `pg_aggregate` and the dependencies. An aggregate can use user transition and final functions (#632).
- `ALTER OPERATOR CLASS` and `ALTER OPERATOR FAMILY` with `RENAME TO`, `OWNER TO` and `SET SCHEMA`, with the checks of `AlterObjectRename_internal`, `AlterObjectOwner_internal` and `AlterObjectNamespace_internal`. A new name or a new schema must not hold a class or a family of the same name for the access method. The moved object depends on its new schema (#631).
- `PARTITION BY` and `CREATE INDEX` take an operator class of the catalog, by its name in the search path or with its schema. A table that `PARTITION BY HASH` partitions with such a class routes and prunes its rows by the support function 2 of the class, as PostgreSQL does. An index depends on the operator classes and the collations of its columns, so `DROP OPERATOR CLASS` and `DROP COLLATION` see it (#630).
- `CREATE OPERATOR FAMILY`, `CREATE OPERATOR CLASS`, `ALTER OPERATOR FAMILY ... ADD` and `... DROP`, and `DROP OPERATOR CLASS` and `DROP OPERATOR FAMILY`, with the checks of `DefineOpClass`, `AlterOpFamily` and the access methods `btree`, `hash`, `gist`, `gin`, `spgist` and `brin`. The new families, classes, operators and support functions are rows of `pg_opfamily`, `pg_opclass`, `pg_amop` and `pg_amproc`, with the same dependencies as in PostgreSQL, so `DROP FUNCTION`, `DROP SCHEMA` and `DROP ROLE` see them. A class in a built-in family gives `0A000` (#629).
- `ALTER OPERATOR ... SET`, `ALTER OPERATOR ... OWNER TO` and `ALTER OPERATOR ... SET SCHEMA`, with the checks of `AlterOperator`, `AlterObjectOwner_internal` and `AlterObjectNamespace_internal`. `SET` can change the estimators, and it can set the commutator, the negator, `MERGES` and `HASHES` when they are not set yet. The operators that a role owns stop `DROP ROLE` (#628).
- `CREATE OPERATOR`, `DROP OPERATOR` and `COMMENT ON OPERATOR`, with the checks of `DefineOperator` and `OperatorCreate`. The function of an operator can be a built-in function or a function in SQL. A commutator or a negator that does not exist yet becomes a shell operator, which a later `CREATE OPERATOR` fills, and `oprcom` and `oprnegate` link the two operators as in PostgreSQL. The new operators are rows of `pg_operator`, and a list of dependencies names them as PostgreSQL does (#627).
- `CREATE FUNCTION` in the languages `internal` and `c`, which only a superuser can use. A function in `internal` runs the built-in function that its `AS` names, and the name must be a built-in function, as `fmgr_internal_validator` checks. A function in C can link to the functions of `regress.so`, the library of the regression tests of PostgreSQL, by its path, and rupg runs `binary_coercible`, `is_catalog_text_unique_index_oid`, `interpt_pp`, `reverse_name`, `test_canonicalize_path`, `test_atomic_ops` and some other functions of that library. Another library or a function that the library does not have gives the error of `fmgr_c_validator`. `LOAD` of that library works. The default `procost` of a function in these languages is 1 (#625).
- A statement checks the privileges that it needs on the tables and the columns that it reads and writes before it runs, as `ExecCheckPermissions` does. A column grant is enough for the columns that the statement uses, the relations of a view count as the owner of the view unless the view has `security_invoker`, and `FOR UPDATE` needs `UPDATE`. (#619)
- `ALTER TABLE ... ADD COLUMN` of a stored generated column, also on a table with children of `INHERITS` and on a partitioned table, where each child gets the expression for its own column numbers. `SET DEFAULT` and `DROP DEFAULT` on a generated column give the errors of `ATExecColumnDefault`, and `tableoid` in a generation expression is the table of the new row (#643).
- A statement checks `EXECUTE` on the functions and the operators that it calls, also in the queries of views, in defaults, in generation expressions and in check constraints, as `ExecInitFunc` does. A default, a generation expression or a check constraint depends on the functions that it calls, so `DROP FUNCTION` of such a function needs `CASCADE` (#643).
- `ALTER TABLE ... ALTER COLUMN ... SET EXPRESSION` and `... DROP EXPRESSION` of a stored generated column, with the checks of `ATExecSetExpression`, `ATPrepDropExpression` and `ATExecDropExpression`. `SET EXPRESSION` computes the column again in each row, and checks the rows against the not-null constraints and the check constraints that read the column (#644).
- `ALTER TABLE ... ADD CONSTRAINT ... NOT VALID` of a check constraint, which does not check the rows that the table has, and `ALTER TABLE ... VALIDATE CONSTRAINT`, which checks them and marks the constraint valid, as `QueueCheckConstraintValidation` does. The children of the table get the constraint as not valid too, and `VALIDATE CONSTRAINT` validates them first (#644).
- A foreign key on a stored generated column gives the errors of `ATAddForeignKeyConstraint` for `ON UPDATE SET NULL`, `SET DEFAULT` and `CASCADE` and for `ON DELETE SET NULL` and `SET DEFAULT`. `SET EXPRESSION` on such a column checks the rows against the foreign key again (#645).
- A partition key cannot use a stored generated column, by its name, in an expression or by a whole-row reference, as `ComputePartitionAttrs` checks (#645).

### Fixed

- A partition key or a check constraint of `CREATE TABLE` can read the whole row of the new table. `ALTER COLUMN ... TYPE ... USING` on a generated column gives `42611`, and `SET EXPRESSION` and `ADD COLUMN` of a generated column give `0A000` when a column of another table uses the row type of the table, as `ATRewriteTables` does (#645).
- A check constraint that reads the whole row of a table with a stored generated column sees the computed value of the column. `pg_get_constraintdef`, `pg_get_indexdef` and `pg_get_expr` show a whole-row reference as `table.*`, as PostgreSQL does, so `ALTER COLUMN ... TYPE` can make such a check constraint again (#644).
- `jsonb_hash` and `jsonb_hash_extended` give the values of PostgreSQL, so a hash index, a hash join and `PARTITION BY HASH` work on `jsonb`. `hash_aclitem_extended` takes a seed that is not 0, and the extended hash of an array or a record of a type without that function names it in the error (#639).
- `CREATE OPERATOR` checks `USAGE` on the argument types and the result type, and `EXECUTE` on the function and the estimators, in the order of `DefineOperator` (#638).
- `ALTER TABLE ... NO INHERIT` on a typed table or on a partition gives the error of `ATPrepChangeInherit` (`42809`) before it looks up the parent, as `ALTER TABLE ... INHERIT` does (#637).
- A function in SQL with a body in a string can call itself, as `ProcedureCreate` adds the function before `fmgr_sql_validator` reads the body. A call of a function in SQL that calls itself with no end gives "stack depth limit exceeded" (`54001`) after 100 levels and does not use all the stack (#636).
- A function that gives a set in the target list stops after the rows that `LIMIT` reads. `SELECT generate_series('2022-01-01'::timestamp, 'infinity', '1 month') LIMIT 10` gave "timestamp out of range", and `SELECT generate_series(1, 2000000000) LIMIT 3` made all the rows before `LIMIT` (#635).
- The plan puts the bodies of the SQL functions `textanycat`, `anytextcat`, `quote_literal(anyelement)` and `quote_nullable(anyelement)` in place of their calls, as `inline_function` does. `EXPLAIN` now shows `((i)::text || 'x'::text)` for `i || 'x'` with an `integer` column, as PostgreSQL does (#634).
- A rename of a function, a procedure or an aggregate by a role that is not its owner gives "must be owner of function", as PostgreSQL does. The rename also checks `CREATE` on the schema for a role that is not a superuser. A view that calls a renamed aggregate shows the new name (#633).
- The check of `CREATE` on a schema reads `nspacl`, so a role that `GRANT CREATE ON SCHEMA` names, or that has the privileges of such a role, can create objects in the schema (#631).
- A query with `HAVING` and no aggregate and no `GROUP BY` does not read `FROM` and does not test `WHERE`, as in PostgreSQL. `make_interval` gives the overflow error of `float8_mul` for a very large `secs`, and a default or a check constraint with a `regrole` or `regdatabase` constant is an error. (#621)
- A column of the target table by a name with the schema in `RETURNING`, such as `RETURNING public.t.id`, now works in `INSERT`, `UPDATE`, `DELETE` and `MERGE`. Before, it gave `42P09` "table reference ... is ambiguous", because the names `old` and `new` of `RETURNING` also matched the table. Prisma sends this form.
- A statement checks the privileges that it needs on the tables and the columns that it reads and writes before it runs, as `ExecCheckPermissions` does. A column grant is enough for the columns that the statement uses, the relations of a view count as the owner of the view unless the view has `security_invoker`, and `FOR UPDATE` needs `UPDATE`. (#619)
- `pg_get_functiondef(oid)` gives the `CREATE OR REPLACE FUNCTION` or `CREATE OR REPLACE PROCEDURE` statement of a function, with the same text as PostgreSQL for the built-in functions and for the functions in SQL and PL/pgSQL with a body in a string. A body in SQL, from `BEGIN ATOMIC` or `RETURN`, gives `0A000` for now. Prisma calls this function when it reads the schema.

### Fixed

- A column of the target table by a name with the schema in `RETURNING`, such as `RETURNING public.t.id`, now works in `INSERT`, `UPDATE`, `DELETE` and `MERGE`. Before, it gave `42P09` "table reference ... is ambiguous", because the names `old` and `new` of `RETURNING` also matched the table. Prisma sends this form (#618).

## 0.1.74 (2026-10-23)

The tag `v0.1.73` has no release, because the check of the release failed on the format of one line. 0.1.74 has all the changes that 0.1.73 was to have.

### Added

- `GRANT` and `REVOKE` of privileges on tables, views, sequences, columns, schemas, functions, procedures, types, domains and languages, with `ON ALL ... IN SCHEMA`, grant options, `CASCADE` and the grantor selection of PostgreSQL. The ACL goes to `relacl`, `attacl`, `nspacl`, `proacl`, `typacl` and `lanacl`, also for a built-in object, and the `has_*_privilege` functions read it. `DROP ROLE` lists the privileges that a role still has, and a dependency list names a procedure as a function, as PostgreSQL does (#614).
- The executor joins first the parts of a `FROM` list that have an `=` between them, and does not make a cross join when a later part has a join condition. The rows keep the order of the written joins (#611).
- `LEAKPROOF` and `NOT LEAKPROOF` in `CREATE FUNCTION` and `ALTER FUNCTION`, with `proleakproof` in `pg_proc` and the error of PostgreSQL for a role that is not a superuser. The `fipshash` functions of the setup of the regression tests use it (#610).
- Deferrable unique constraints and primary keys. A new row with the key of another row is checked again at the end of the statement, or at the commit when the constraint is deferred, as `unique_key_recheck` does it. The constraint gets its internal trigger, `ON CONFLICT` with a deferrable arbiter gives the error of PostgreSQL, and a rollback to a savepoint removes the checks of the subtransaction (#609).

### Changed

- The 42 crates of `crates/` can be published to crates.io. Before, each crate had `publish = false`. `rupg-types` now reads `tzdata.zi` and the `Default` time zone abbreviations from copies in its own directory, which `cargo xtask tzdata` makes, because a published crate cannot read `vendor/` (#612, #615).

## 0.1.72 (2026-10-23)

This release checks unique indexes on expressions and partial unique indexes in the writes, uses indexes on expressions and partial indexes in `EXPLAIN`, plans the join of `MERGE` with the target table on the left side, takes out left joins that read no column of the inner side, turns `IN` on a correlated subquery into a semi join, and shows the `Output` of `RETURNING` and `MERGE` as PostgreSQL does.

### Added

- `INSERT`, `UPDATE`, `COPY FROM`, `MERGE` and `CREATE UNIQUE INDEX` check unique indexes on expressions and partial unique indexes. `ON CONFLICT` infers these indexes from an expression and a `WHERE` clause, and the detail of a duplicate key shows the expressions of the index. (#607)
- `EXPLAIN` uses indexes on expressions and partial indexes. An index on an expression matches a condition, an order and a join condition of the same expression. A partial index is used when the conditions imply its predicate, and its estimates use the rows of the index. `=` of the expression of a unique index passes one row. (#606)
- The plan of a `MERGE` has the target table on the left side of the join, as PostgreSQL makes it. With `NOT MATCHED BY SOURCE`, the plan no longer shows a `Values Scan` of the marker of the source rows, and the rows carry the whole row of the source, as in PostgreSQL. (#605)
- `EXPLAIN VERBOSE` shows `old`, `new` and `MERGE_ACTION()` of `RETURNING` in the `Output` of the `ModifyTable`, as PostgreSQL does. Before, the `Output` line was missing. (#604)
- `EXPLAIN` takes out a left join that gives one row at most for each outer row when the query reads no column of the inner side, as `remove_useless_joins` of PostgreSQL 19 does. A nested loop gives the columns of a join clause that its parameterized inner scan tests, so the widths and the `Output` of the two sides match PostgreSQL. (#603)
- `EXPLAIN` turns `IN` on a correlated subquery into a semi join and merges the subquery, as PostgreSQL 19 does, and it merges a simple `LATERAL` subquery of `FROM`. The scans below a hash join or a merge join give the columns in the order of the conditions in the join tree. (#602)
- `EXPLAIN` of an `UPDATE`, a `DELETE`, a `MERGE` or an `INSERT` merges the simple subqueries and turns `EXISTS`, `NOT EXISTS` and `IN` into joins, as PostgreSQL does, also for a table with check constraints. The one-row `VALUES` of an `INSERT` into such a table shows its values in place of nulls. (#601)
- `EXPLAIN VERBOSE` of an `INSERT`, an `UPDATE`, a `DELETE` and a `MERGE` with `RETURNING` now shows the list of `RETURNING` as the `Output` of the `ModifyTable`, as PostgreSQL does. The node below now gives the columns of the other relations that `RETURNING` reads, after the row marks. (#600)
- `EXPLAIN VERBOSE` of a `MERGE` now shows the targets that PostgreSQL gives the node below the `Merge`: the ID of the version of the row of the target table, the columns of the source that the `WHEN` clauses read, and the ID of the version of the row of the source. The columns of the target table and the marker of the source row do not show. (#599)

## 0.1.71 (2026-10-23)

This release adds PlaceHolderVar for the targets of a subquery below the nullable side of an outer join, counts the keys of the first window as the order that the query asks for, costs only the sampled pages of `TABLESAMPLE SYSTEM`, and shows the rows and the width of writes with `RETURNING`.

### Added

- `EXPLAIN` of `TABLESAMPLE SYSTEM` now costs only the sampled pages, at the cost of a random page, as PostgreSQL does, and `REPEATABLE` shows on the `Sampling` line in the text form. `INSERT`, `UPDATE` and `DELETE` with `RETURNING` now show the rows and the width of the node below them. (#597)
- The keys of the first window of a query now count as the order that the query asks for, as `window_pathkeys` in PostgreSQL, so a window under a `LIMIT` can read an index in the order of its first keys, with an `Incremental Sort` for the rest. The `Window` of a `WindowAgg` in `EXPLAIN` now shows no direction for its keys, as PostgreSQL shows it. (#596)
- `EXPLAIN` of a query that merges a subquery on the side of an outer join that can give nulls now shows the columns of the subquery that are not plain columns as `PlaceHolderVar`s, as PostgreSQL does: the scan or the join below computes the value, the nodes above read it in parentheses, and the costs, the widths and the number of groups match. A constant column in the `ON` of that join now folds, and a `CASE` in `EXPLAIN` now takes one line. (#595)
- A `btree` index of a table with rows has two pages for the planner, the metapage and the root, as in PostgreSQL, so the cost of a nested loop over a scan of the index is the cost of PostgreSQL. (#594)

## 0.1.70 (2026-10-23)

This release plans grouping sets with rollups and MixedAggregate, plans a Group node for GROUP BY with no aggregate, and allows the columns of a table whose primary key is in GROUP BY. The scan of a subquery reads each plan of the subquery with its order, and UNION can use a Unique above a Merge Append.

### Added

- The plan of grouping sets is the one of PostgreSQL: the sets that one sort gives form a rollup, and the `Agg` hashes the sets, sorts the rows once for each rollup, or does both in a `MixedAggregate`, whichever costs less. `EXPLAIN` shows the `Sort Key` of each rollup after the first one. (#592)
- A column of a table whose primary key is in `GROUP BY` can be in the targets and in `HAVING` without an aggregate, as PostgreSQL allows it. A view of such a query depends on the primary key. (#591)
- `GROUP BY` with no aggregate is a `Group` node of a sorted input in `EXPLAIN`, with the cost that PostgreSQL gives it, in place of a `GroupAggregate`. (#590)
- The scan of a subquery of `FROM` reads each plan of the subquery that the planner keeps, with the order of its rows, as PostgreSQL does. `ORDER BY` and a merge join above a subquery need no sort when the subquery gives its rows in their order, and a `UNION` in `FROM` can use a `Unique` above a `Merge Append`. The scan of a query of `WITH` has the order of the rows of its plan. A subquery with `FOR UPDATE` keeps the cost of its `Subquery Scan`. (#589)
- `UNION` without `ALL` also plans a `Unique` above a `Merge Append` of its `SELECT`s in the order of the columns, as PostgreSQL does. A `Unique` of a sort or of a `Merge Append` gives the rows in the order of the columns, so `ORDER BY` of the columns needs no other sort, also when a `SELECT` casts a column. (#588)
- A constant of a `SELECT` of a set operation with another type than the operation keeps its cast above the `SELECT`, so the plan has the `Subquery Scan` of PostgreSQL, and a sorted `SetOp` does not sort the constant (#587).

## 0.1.69 (2026-10-23)

This release plans a Merge Append for a UNION ALL with ORDER BY, and follows the enable_* planner settings of PostgreSQL 19 with disabled nodes, as EXPLAIN shows them.

### Added

- `EXPLAIN` now follows `enable_material` and `enable_presorted_aggregate`. An aggregate call with `ORDER BY` in a query with no `GROUP BY` now reads an index in the order of the call when that costs less than a sort, as PostgreSQL plans it. (#585)
- `EXPLAIN` now follows the planner settings `enable_seqscan`, `enable_indexscan`, `enable_indexonlyscan`, `enable_bitmapscan`, `enable_sort`, `enable_incremental_sort`, `enable_hashagg`, `enable_nestloop`, `enable_mergejoin` and `enable_hashjoin`. A node of a kind that a setting turns off is a disabled node, and the planner takes the plan with the fewest disabled nodes, as PostgreSQL does. `EXPLAIN` shows `Disabled: true` for a disabled node. (#584)
- `EXPLAIN` of a `UNION ALL` with `ORDER BY` now plans a `Merge Append` of the plans of its `SELECT`s when that costs less than a sort, as PostgreSQL makes the query an append relation. So `SELECT id FROM a UNION ALL SELECT id FROM b ORDER BY 1 LIMIT 3` reads the primary keys in order. A `SELECT` of a `UNION ALL` with its own `WHERE` gets a sort below the `Merge Append`, as PostgreSQL plans it as a subquery. (#583)

## 0.1.68 (2026-10-23)

A patch release on the way to M3. `EXPLAIN` now plans a `Merge Append` for a `UNION ALL` in `FROM` that the query reads in order, plans `INTERSECT` and `EXCEPT` with the hashed and the sorted `SetOp`, and shows the `Subquery Scan` and the `Result` that add the casts of the columns of a set operation.

### Added

- `EXPLAIN` of a set operation whose `SELECT` needs a cast to the column types of the operation shows the `Subquery Scan` that adds the casts, with its cost, as PostgreSQL plans it. A set operation below another one with other column types gets a `Result` that adds the casts, and the width of an `Append` is the average width of its members. (#581)
- `EXPLAIN` now plans `INTERSECT` and `EXCEPT` as PostgreSQL does. The planner makes a `HashSetOp` and a `SetOp` with `Strategy: Sorted`, which reads its sides in the order of their columns, and keeps the plan that costs less. Each `SELECT` of a side is planned in that order, so an index scan can take the place of a sort. `INTERSECT` reads first the side with fewer groups, the number of groups of a side comes from the statistics of its columns, and a hash table that does not fit in `hash_mem` is not a choice. The rows of the sorted `SetOp` take the place of a sort for `ORDER BY`, and with `LIMIT` the planner can use the sorted `SetOp`, which gives its first rows sooner (#580).
- `EXPLAIN` plans a `Merge Append` for a `UNION ALL` in `FROM` when the query reads its rows in an order and that costs less than a sort of the `Append`, as `generate_orderedappend_paths` does. The planner keeps the plans of each `SELECT` in the orders that the query can use, a `SELECT` with no plan in the order gets a sort, and each `SELECT` reads only the columns that the query reads, so that an index only scan can give them (#579).

## 0.1.67 (2026-10-23)

A patch release on the way to M3. `EXPLAIN` now plans semi joins and anti joins for `EXISTS`, `IN` and `NOT IN`, merges the simple subqueries of `FROM` and moves the conditions of `WHERE` into them, plans index scans, index only scans and bitmap scans with parameterized index scans in nested loops, plans `min` and `max` of an indexed column with an index, and plans a `UNION ALL` in `FROM` as an `Append` of its `SELECT`s.

### Added

- `EXPLAIN` now plans a `UNION ALL` in `FROM` as an `Append` of its `SELECT`s with no `Subquery Scan` above it, as PostgreSQL makes it an append relation. The conditions of `WHERE` go into each `SELECT`, so `WHERE id = 1` gives an index scan of each table. A `UNION` and an `INTERSECT` in `FROM` also take the conditions into each `SELECT`. The width of the `Append` counts only the columns that the query reads, and a `Result` above it computes the targets that are not columns. The keys of a node above the `Append` show with the names of the first `SELECT`, and a `UNION` uses the sorted rows when the hash table does not cost less by more than the fuzz factor of `add_path` (#577).
- A subquery with no `FROM` in `FROM` now merges into the query in `EXPLAIN`, as PostgreSQL merges it, so `SELECT * FROM (SELECT 1 AS k) s, a WHERE a.id = s.k` gives an `Index Scan` with no `Nested Loop` and no `Result`. A condition that runs once in a `Result` above a join now makes the nested loop cost as PostgreSQL costs it, and the node below the `Result` gives only the columns that the `Result` reads (#576).
- `EXPLAIN` shows the `min` and `max` of an indexed column as a `Result` with `Replaces: MinMaxAggregate` and an `InitPlan` for each call, with a `Limit` over an index scan in the order of the aggregate, as PostgreSQL plans it ([#575](https://github.com/tamnd/rupg/pull/575)).
- `EXPLAIN` now shows a nested loop whose inner side is a parameterized index scan or bitmap heap scan, with the join condition as the `Index Cond` or the `Recheck Cond`. The planner follows `create_index_paths`, `add_path` for parameterized paths and `final_cost_nestloop`, so the plans and costs match PostgreSQL. (#574)
- The conditions of `WHERE`, `ON` and `HAVING` are canonical after they fold, as `canonicalize_qual` makes them. An `OR` drops an arm that is null, and an `AND` with an arm that is null is false. A condition that each arm of an `OR` has goes out of the `OR`, so `(u.x = v.a AND u.y = 1) OR (u.x = v.a AND v.b = 2)` is `(u.x = v.a) AND ((u.y = 1) OR (v.b = 2))`, and the join can use `u.x = v.a` as its hash or merge condition ([#573](https://github.com/tamnd/rupg/pull/573)).
- `EXPLAIN` shows `BitmapOr` of the bitmaps of the arms of a condition that is an `OR`, as `generate_bitmap_or_paths` makes it, with the costs of `cost_bitmap_or_node`. The arms of an `OR` that compare one column of an index with one operator are `x = ANY (array)`, as `group_similar_or_args` and `match_orclause_to_indexcol` make it, so `a = 1 OR a = 2` gives the index condition `(a = ANY ('{1,2}'::integer[]))`. This also works for an `Index Scan`. The heap scan has the `OR` as `Recheck Cond`, and as `Filter` when the conditions of the indexes do not prove it, as `predicate_implied_by` ([#572](https://github.com/tamnd/rupg/pull/572)).
- `EXPLAIN` uses the fixed prefix of `LIKE`, `ILIKE`, a regular expression or `^@` as conditions of a btree index, as `match_pattern_prefix` does. `c LIKE 'x%'` gives the index condition `(c >= 'x'::text) AND (c < 'y'::text)`, and a pattern with no wildcard gives `=`. The pattern stays in the filter. This works for `text`, `varchar`, `char`, `name` and `bytea`, and for the operator classes `text_pattern_ops` and `bpchar_pattern_ops`. A btree index on a column of `name` can now test the conditions of the column ([#571](https://github.com/tamnd/rupg/pull/571)).
- `EXPLAIN` shows `Bitmap Heap Scan` of a table with `Bitmap Index Scan` of its btree indexes, and `BitmapAnd` of two indexes or more, as `choose_bitmap_and` picks them. The costs are those of `cost_bitmap_heap_scan` and `compute_bitmap_pages`. The heap scan shows the conditions of the indexes as `Recheck Cond` and the other conditions as `Filter` ([#570](https://github.com/tamnd/rupg/pull/570)).
- `EXPLAIN` shows `Index Scan` and `Index Only Scan` of a table by its btree index, forward or backward, as `create_index_paths` makes them. The costs are those of `btcostestimate` and `cost_index`. The index gives the sort order for `ORDER BY`, `LIMIT`, `GROUP BY` and joins, and an index that has all the columns of the query gives an index only scan ([#569](https://github.com/tamnd/rupg/pull/569)).
- `EXPLAIN` puts a `Result` above the sort when a target is not a key of the query and calls a volatile function, or costs more than 10 times `cpu_operator_cost` with `LIMIT`, as `make_sort_input_target` does. The sort gives only the other targets and the columns of these targets, so its rows are narrower, and the `Result` costs the targets and `cpu_tuple_cost` for each row. A node that can compute its targets, such as an `Agg`, computes them itself. A sort of no rows now rounds as the `LOG2` of PostgreSQL, and a scan with a condition that is never true has the width of its targets when a target is not a column ([#568](https://github.com/tamnd/rupg/pull/568)).
- `EXPLAIN` counts the cost of targets that call functions, as `set_pathtarget_cost_width` does. The scan of `SELECT x + 1 FROM a` costs 25.00 in place of 22.00. The plan of `FROM` computes the targets before the sort, or the keys of `GROUP BY` with groups, or the keys of the windows with windows, and each scan of a partitioned table computes them. The `Agg` node, the last `WindowAgg` node and the `HashAggregate` of `DISTINCT` add the cost of their targets, and an aggregate call or a window call costs nothing in a target or in `HAVING`. A target that is volatile, or that costs more than 10 times `cpu_operator_cost` with `LIMIT`, is not counted below the sort, as `make_sort_input_target` postpones it ([#567](https://github.com/tamnd/rupg/pull/567)).
- `x IN (VALUES (1), (2))` and the other `ANY` on a list of `VALUES` in `WHERE` and in the `ON` of an inner, a left or a right join become `x = ANY ('{1,2}')`, as `convert_VALUES_to_ANY` does, when the list has two rows at least and each value folds to a constant. The query then tests a constant array in place of a semi join or a subquery ([#566](https://github.com/tamnd/rupg/pull/566)).
- The planner makes a condition of one relation from an `OR` of a join, as `extract_restriction_or_clauses` does. For `(a.x = 42 AND b.y = 43) OR (a.x = 44 AND b.z = 45)` the scan of `a` tests `a.x = 42 OR a.x = 44` and the scan of `b` tests `b.y = 43 OR b.z = 45`, when the new condition passes 0.9 of the rows at most. The join still tests the `OR`, with its selectivity for an inner join divided by the selectivity of each new condition ([#565](https://github.com/tamnd/rupg/pull/565)).
- The planner estimates `LIKE`, `ILIKE`, `NOT LIKE`, the regular expression operators and `^@` of a column and a constant pattern as `patternsel` does with no statistics. A pattern that has no wildcard is `=` of its value, and the other patterns take 0.005 for a fixed prefix times the selectivity of the rest of the pattern, so that `LIKE '%google%'` passes fewer rows than `LIKE 'abc%'` ([#564](https://github.com/tamnd/rupg/pull/564)).
- `EXPLAIN` marks a join with a condition that is never true as dummy, as `populate_joinrel_with_paths` does. A part of `ON` of an inner join with no column is now a condition of the join of all the relations, so `ON false` gives a `Result` with no rows in place of the join, and another such part is a `One-Time Filter` above the join. A left join or an anti join with a part of `ON` that is never true has an inner side with no rows that keeps its width, and an inner join below an outer join with such a part is a `Result` with no rows. A nested loop and a merge join count an inner side with no rows as one row, as `final_cost_nestloop` does, and the scan of the only relation counts the cost of a condition with no column in its own start cost. Before, the planner tested the condition on each row of the join. (#563)
- The planner moves a `NOT` into its argument, as `negate_clause` of `eval_const_expressions`. An operator with a negator becomes the negator, also in `ANY` and `ALL`, `AND` and `OR` change places, and `IS NULL` and `IS TRUE` take the opposite test. A test of `IS NULL` of a `ROW` expression becomes a test of each field. `EXPLAIN` finds the selectivity of `IS TRUE` and the other boolean tests as `booltestsel`, and of `IS DISTINCT FROM` as the rest of its `=` operator. A relation with a condition that is never true has a width of 0, and a query with no `FROM` and such a condition has one row, as in PostgreSQL. (#562)
- `EXPLAIN` finds the rows of a join with an `OR` in its condition from the selectivity of each part of the `OR`, as `clause_selectivity` does. An `=` of a column of each side is `eqjoinsel`, and an `=` of a unique column and a constant passes one tuple of the relation, as `var_eq_const`. Before, the join used the default selectivity for each `=` in an `OR`. (#561)
- `EXPLAIN` plans left joins inside the join search of their join domain, as `join_is_legal` lets PostgreSQL do it. An inner join can now come before or after a left join, as identity 1 of `optimizer/README`. A condition above a left join that reads its inner side waits for the left join, as the left join is in its `varnullingrels`. When the condition of a left join, a semi join or an anti join reads the inner side of a lower left join, the two joins keep their order, as `make_outerjoininfo` finds it. Two left joins do not change their order yet (#560).
- `EXPLAIN` plans semi joins and anti joins inside the join search of their join domain, as `join_is_legal` lets PostgreSQL do it. The inner side of each one is one item of the domain, and its outer side is the relations that its conditions read. So the planner can join a semi join or an anti join before an inner join, put two anti joins in the best order, and join the unique rows of the inner side of a semi join with any relation. A semi join whose inner side is unique for its conditions becomes an inner join in the same search, as `reduce_unique_semijoins`. The conditions of `WHERE` now come before the conditions of the semi joins of its subqueries in the order of `deconstruct_jointree`, so that the members of an equivalence class are in the same order as in PostgreSQL (#559).
- A left join with a test of `IS NULL` above it as an anti join in `EXPLAIN`, as `reduce_outer_joins` of PostgreSQL makes it, such as `c LEFT JOIN b ON b.x = c.x WHERE b.x IS NULL`. The column cannot be null in a row that the join matches: the condition of `ON` is false or null when the column is null, or the column has `NOT NULL` in its table and no outer join below gives nulls for it. The test of `IS NULL` goes away, and a right join becomes an anti join with its sides in the other order (#558).
- `NOT IN` on a subquery as an anti join in `EXPLAIN`, as `convert_ANY_sublink_to_join` of PostgreSQL 19 makes it. The planner makes the anti join only when no value of the test and no output of the subquery can be null: a column of a table with `NOT NULL` or a primary key, a constant, or a column that a condition of the subquery such as `x IS NOT NULL` or `x > 0` proves not null. A column on the side of an outer join that can give nulls stays a subplan (#557).
- Semi joins and anti joins in `EXPLAIN`, as `pull_up_sublinks` of PostgreSQL makes them. Each part of `WHERE` that is `EXISTS`, `NOT EXISTS` or `IN` on a subquery that reads columns of the query above becomes a `Semi Join`, an `Anti Join`, a `Right Semi Join` or a `Right Anti Join`, with the selectivity of `eqjoinsel_semi`. A semi join can also be an inner join with the unique rows of the inner side, from a `HashAggregate` or from a `Unique` over a `Sort`, as `create_unique_paths` makes them. A semi join with a primary key on the inner side is an inner join, as `reduce_unique_semijoins`. Two columns of one table in an equivalence class give a condition of the scan, such as `(x = y)` for `(x, y) IN (SELECT x, x FROM c)` (#556).
- The subqueries of `FROM` that the planner merges into the query above them in `EXPLAIN`, as `pull_up_subqueries` of PostgreSQL does it. A subquery with no aggregate, window, `ORDER BY`, `DISTINCT`, `LIMIT` or row lock becomes the relations of its `FROM`, in the list of `FROM` and in a side of a join, and its `WHERE` joins the `WHERE` of the query or the `ON` of the join. `SELECT * FROM (SELECT x FROM b) s` is a `Seq Scan on b` with no `Subquery Scan`, and `EXPLAIN VERBOSE` names its columns as `b.x` (#547).
- The conditions of `WHERE` that the planner moves into a subquery of `FROM` in `EXPLAIN`, as `set_subquery_pathlist` and `subquery_push_qual` of PostgreSQL do it. A condition that reads only the subquery joins the `WHERE` of the subquery, or its `HAVING` when the subquery makes groups, and the scan of the subquery does not test it again. A condition stays outside a subquery with `LIMIT` or `OFFSET`, and a condition stays outside when it reads a column of a window function, a column that is not an item of `DISTINCT ON` or of each `PARTITION BY`, or a column that calls a volatile function or a function that gives a set. The planner makes an outer join an inner join before it moves the conditions, as `reduce_outer_joins` (#550).

### Fixed

- The plans of the subqueries of `EXISTS`, `ANY` and `ALL` in `EXPLAIN`. PostgreSQL plans `EXISTS` to read one row, and `ANY` and `ALL` to read half the rows, as `make_subplan` does it. The joins of the subquery keep the plans with a lower start cost, and the subplan runs the plan that costs the least for the rows that it reads, such as a `Nested Loop` that gives its first row at once in the place of a `Hash Join`. `LIMIT` in the subquery changes the fraction, as `preprocess_limit` (#555).
- The order of the two sides of an `=` condition of a join that a `Nested Loop` tests in `EXPLAIN`, such as `(b.y = c4.x)` for `ON c4.x = b.y`. PostgreSQL makes the condition again from its equivalence class with the column of the first relation on the left, also for a class of one condition. Each join of the relations of a class also gives each column of the class, which changes the width of the rows (#555).
- `EXPLAIN` of a correlated `EXISTS` that PostgreSQL runs as `ANY` on a hash table, such as `(ANY (x = (hashed SubPlan exists_to_any_1).col1))`. As `convert_EXISTS_to_ANY`, each condition of the subquery that compares a column of the query with an operator that can hash becomes a column of the subquery. The plan keeps the subplan of `EXISTS` or of `ANY` with the lower cost for the rows of its node, as `fix_alternative_subplan`. The subquery of `EXISTS` also loses its `ORDER BY`, `DISTINCT`, `GROUP BY` and `LIMIT`, as `simplify_EXISTS_query` (#554).
- The cost of a condition that reads no column of its query in `EXISTS`, such as `c.x = 7`, in `EXPLAIN`. PostgreSQL runs it once in the `One-Time Filter`, and charges it to the start cost of the node below (#554).
- The rows and the cost of `x op ANY (array)` and `x op ALL (array)` in `EXPLAIN`, such as `x IN (1, 2, 3)`. `scalararraysel` of PostgreSQL finds the selectivity of the operator for each item, and adds them for `= ANY` and `<> ALL`. An array that is not a constant or a list has 10 items. The operator runs for half of the items for each row, and a constant array of 9 items or more is a hash table that costs one hash for each item when the scan starts, as `convert_saop_to_hashed_saop` makes it (#553).
- The start cost of the conditions of an aggregate with `HAVING` and of the filter of a join in `EXPLAIN`. `cost_agg` and the costs of the joins of PostgreSQL add it to the start cost of the node (#553).
- The total cost of a `Seq Scan` with a start cost in `EXPLAIN`. `cost_seqscan` of PostgreSQL adds the cost of the pages last, which the rounding of the sum follows (#553).
- The cost of the conditions with a subquery that runs as a `SubPlan` for each row in `EXPLAIN`, as `cost_subplan` of PostgreSQL finds it. A hashed subplan costs its plan once when the node starts, and a correlated subplan costs its plan for each call, half of it for `ANY` and `ALL`. The conditions of a scan are in the order of their cost, so a condition with a subplan comes last. A `HAVING` and the filter of a join cost their subplans in the same way (#552).
- The choice of a `HashAggregate` or a `GroupAggregate` for `GROUP BY` in `EXPLAIN`. As `add_path` of PostgreSQL does, the plan keeps the hash table when its total cost is lower by more than the fuzz factor, or when its total cost is near and its start cost is lower by more than the fuzz factor (#552).
- The names of the columns of a `Subquery Scan` that the plan does not keep in `EXPLAIN`. A node above the scan shows the expression of the subquery, such as `b.x` or `(count(*))`, in place of `s.x`, as `get_variable` of PostgreSQL finds it through the target list of the node below (#551).
- `Inner Unique: true` in `EXPLAIN VERBOSE`, and the cost of a join whose inner side is a subquery that gives each row once on the columns of the join, as `innerrel_is_unique` and `query_is_distinct_for` of PostgreSQL find it. The subquery has `DISTINCT`, `GROUP BY`, an aggregate call with no `GROUP BY`, or a set operation with no `ALL` (#551).
- The cost of a `Subquery Scan` whose columns the query reads in a different order from the subquery, such as `SELECT n, x FROM (SELECT x, count(*) AS n ...) s`. `set_subquery_pathlist` of PostgreSQL finds the target of the scan trivial only when it gives each column of the subquery in order (#551).
- The cost of a `Subquery Scan` that has no condition and gives each column of its subquery in `EXPLAIN`. `cost_subqueryscan` of PostgreSQL gives it the cost of its subquery, because the plan does not keep the node (#550).
- The cost of the subplans that run once in `EXPLAIN`, as `SS_charge_for_initplans` of PostgreSQL adds it to the start cost and the total cost of the top node of the query. Each `InitPlan` and each `CTE` costs its plan, and an `InitPlan` of `EXISTS` costs its first row (#549).
- The cost of a `CTE Scan` and of a `WorkTable Scan` in `EXPLAIN`. `cost_ctescan` of PostgreSQL charges two `cpu_tuple_cost` for each row (#549).
- The selectivity of a condition that is a column, a parameter or a subplan in `EXPLAIN`, such as `(InitPlan exists_1).col1`. `boolvarsel` of PostgreSQL gives half of the rows with no statistics. A row comparison with `=` and a subquery that runs once is the comparisons of its columns (#549).
- The cost of a row comparison such as `ROW(x, y) < ROW(1, 2)` in `EXPLAIN`, which is one operator for each column, as `cost_qual_eval` of PostgreSQL finds it (#549).
- A `Subquery Scan` in `EXPLAIN` stays when its subquery gives a junk column, such as a key of `ORDER BY` that is not a target, as `trivial_subqueryscan` of PostgreSQL finds it (#549).
- The rows of a low bound and a high bound of the same value in `EXPLAIN`, such as `x > 1 AND x < 5` and `x BETWEEN 1 AND 5`. `clauselist_selectivity` of PostgreSQL estimates the two conditions as one range with the selectivity `DEFAULT_RANGE_INEQ_SEL`, so that the scan of a table of 2260 rows gives 11 rows and not 251 (#548).
- The rows of a join on `=` in `EXPLAIN` when a side has conditions of its own. `eqjoinsel` of PostgreSQL does not make the number of distinct values of a column smaller than the rows of its table after the conditions (#547).
- The junk columns of the row marks in `EXPLAIN`, as `preprocess_rowmarks` and `preprocess_targetlist` of PostgreSQL add them. `FOR UPDATE` and `FOR SHARE` carry the `ctid` of each table of `FROM`, the `tableoid` of a table with an inheritance tree and the whole row of a function, and an `UPDATE` or a `DELETE` carries the `ctid` of each table other than its target. The widths and the `Output` lines of `EXPLAIN VERBOSE` show these columns (#546).
- The width of a whole-row column in `EXPLAIN`, such as `SELECT b FROM b`. PostgreSQL adds the header of the row to the width of its columns (#546).

## 0.1.66 (2026-10-22)

A patch release on the way to M3. `EXPLAIN` now uses each plan of the join for the nodes above it, shows `Incremental Sort`, estimates the groups as PostgreSQL does with no statistics, and leaves out the keys of `GROUP BY`, `DISTINCT`, `ORDER BY` and `PARTITION BY` that add nothing.

### Added

- The plans of the join of `FROM` in the nodes above it in `EXPLAIN`, as `grouping_planner` reads the `pathlist` of PostgreSQL. `ORDER BY`, `GROUP BY`, `DISTINCT`, the windows and `LIMIT` go on each plan of the join that the planner keeps, and the result with the lowest total cost wins. So a merge join whose rows are in the order of `ORDER BY` has no sort above it when that costs less than the cheapest join with a sort. A plan that is not the cheapest is used only when its rows are in the order of each node above it, as `create_ordered_paths` does it. (#540)
- `Incremental Sort` in `EXPLAIN`, as `create_ordered_paths` and `make_ordered_path` of PostgreSQL use it. When the rows of a plan are in the order of the first keys of `ORDER BY`, `GROUP BY`, `DISTINCT` or a window, the node sorts only each group of rows with the same values of these keys. The cost is the cost of `cost_incremental_sort`, and the node shows the `Sort Key` and the `Presorted Key` in each format. (#541)
- The number of groups of `GROUP BY`, `DISTINCT`, the windows and `Incremental Sort` in `EXPLAIN`, as `estimate_num_groups` of PostgreSQL finds it with no statistics. Two or more columns of one table have at most a tenth of the rows of the table as groups, or the most values of one of the columns when that is more. A table whose conditions pass fewer rows than it has gives fewer groups. Two columns that an equivalence class makes equal count once, and a `boolean` expression has 2 groups. (#542)
- The keys of `GROUP BY`, `DISTINCT` and `ORDER BY` in `EXPLAIN` without the keys that add nothing, as `pathkey_is_redundant` and `remove_useless_groupby_columns` of PostgreSQL find them. A key whose equivalence class has a constant goes, such as `y` with `WHERE y = 1`, and so does a key in the class of an earlier key. A column of `GROUP BY` goes when the other columns of its table in `GROUP BY` have all the columns of a unique index whose columns are `NOT NULL`, such as a primary key. A `DISTINCT` with no keys left shows a `Limit` that takes the first row (#543).
- The items of `PARTITION BY` of a window in `EXPLAIN` without the items that add nothing, as `make_pathkeys_for_window` of PostgreSQL removes them. The `Window` line, the sort and the costs of the `WindowAgg` use only the other items (#544).

### Fixed

- The order of the windows in `EXPLAIN` when two windows have the same columns in a different order. PostgreSQL numbers the items of `ORDER BY` of a window before its items of `PARTITION BY`, and `select_active_windows` sorts the windows by these numbers (#544).
- The `Output` of the node below a `WindowAgg` in `EXPLAIN VERBOSE` has the columns of the windows in the order of the target list, as `make_window_input_target` of PostgreSQL makes it (#544).
- The start cost of a `WindowAgg` in `EXPLAIN`, as `get_windowclause_startup_tuples` and `cost_windowagg` of PostgreSQL 19 find it. The window reads one more row than its frame before its first row, and `row_number`, `rank` and the other functions of the rank of a row use the frame `ROWS UNBOUNDED PRECEDING`, as `optimize_window_clauses` gives it. (#540)
- The cost of the final function of an aggregate call in `EXPLAIN`, as `get_agg_clause_costs` adds it for each group. (#541)
- The limit of the rows of a sort for `FETCH FIRST ... WITH TIES` in `EXPLAIN`. PostgreSQL uses the count and the offset as the bound of the sort also with `WITH TIES`. (#541)
- The cost of an aggregate call that a query has two times, such as `count(*)` in the targets and in `HAVING`, in `EXPLAIN`. PostgreSQL computes the two calls as one call. (#542)

## 0.1.65 (2026-10-22)

### Added

- Conditions of joins from equivalence classes with no constant in `EXPLAIN`, as `generate_join_implied_equalities` makes them in PostgreSQL. When the `=` conditions of a query make three or more columns of different tables equal, each join gets one condition of the class, between the first column of each side, also for two tables that the query does not compare. A join of three tables on one value now has the estimate of one condition and not of two (#536).
- The plans of each set of tables in the join search of `EXPLAIN`, as the `pathlist` of PostgreSQL. A merge join can read a plan of a side that is not its cheapest plan when that plan is in the order of the merge, as `generate_mergejoin_paths` does it. A join that is an outer side tries each plan of the outer side. The number of rows of an estimate that ends in a half now goes to the even number, as `rint` does it in `clamp_row_est`, so a join of four tables on one value has the same plan as in PostgreSQL (#537).
- Unique indexes in the estimates of `EXPLAIN`, as PostgreSQL uses them. A column with a unique index of one key column and no predicate has as many distinct values as the table has rows, so `x = 5` on that column passes one row, `x <> 5` passes all the rows but one, and an `=` join on the column has the estimate of PostgreSQL. A join whose inner side is a table with a unique index that is immediate and has no predicate, with an `=` condition on each key column, stops at the first match. Its hash join, nested loop and merge join cost less, as `innerrel_is_unique` makes them (#538).

## 0.1.64 (2026-10-22)

### Added

- Join search in `EXPLAIN`, as PostgreSQL finds the order of the joins. The tables of a `FROM` list and of inner joins join in the order with the lowest cost, first by the conditions of the joins and by the equivalence classes, then a table with no condition, and a function with a lateral reference joins after the tables that it reads. A left join or a full join is one item of the search (#531).
- The width of the rows when `EXPLAIN` makes the plan, as PostgreSQL finds it from the columns that the nodes above each scan and each join read. A sort that does not fit in `work_mem` now has the cost of a merge sort on disk, a `Materialize` that does not fit writes its rows to disk, a merge join puts a `Materialize` over a sorted inner side that does not fit, and a hash join with a large inner side runs in batches. The conditions of a merge join and of a hash join now cost as much as their expressions, and the `Materialize` of a merge join costs one `cpu_operator_cost` for each row, as in PostgreSQL. A hash join whose inner side has no statistics and does not fit in `hash_mem` costs `disable_cost` more, an I/O cast costs 2 function calls, and an expression of one small relation has as many distinct values as the relation has tuples (#532).
- The plans that `EXPLAIN` keeps for each join, as `add_path` keeps them in PostgreSQL. Two costs that differ by less than 1% are the same cost, so a hash join with a lower start cost wins over a merge join that costs a little less in total. A merge join stays when the order of its rows is of use to `ORDER BY`, `GROUP BY`, `DISTINCT` or a merge join above it (#533).
- The order of the keys of a merge join in `EXPLAIN`, as PostgreSQL chooses it. When the conditions of the join have the keys of `GROUP BY`, `DISTINCT` or `ORDER BY`, the join sorts its sides in that order and with the same directions, also `DESC` and `NULLS FIRST`, and the plan has no `Sort` above the join. The other keys follow, first the keys with more tables outside the join that are equal to them. A side that is in the order of the keys already has no `Sort`, and a key of `ORDER BY` that is equal to an earlier key needs no `Sort` (#534).

## 0.1.63 (2026-10-22)

### Added

- `EXPLAIN` of a partitioned table and of a table with children of `INHERITS`. The plan has an `Append` of a scan for each leaf partition that the conditions of `WHERE` do not prune, or for the table and each of its children, with the names of PostgreSQL such as `r_1`. One leaf gives one scan with no `Append`, and no leaf gives a `Result` with `One-Time Filter: false`. `UPDATE` and `DELETE` show each target table. The estimates are the ones of PostgreSQL, also for a table with children, which has no pages. Two conditions of `=` on a partition key with no common value now prune all the partitions (#528).
- Partition pruning. A scan of a partitioned table reads only the leaf partitions that can have rows that pass the conditions of `WHERE` on the partition key, as PostgreSQL finds them: `=`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IN`, `= ANY` of a constant array, `IS NULL` and `OR` of these, with constants and the parameters of `EXECUTE` and `Bind`. The default partition is pruned when the other partitions have all the values that the conditions let the key have. `pg_stat_user_tables` now counts the scans of only those leaves, as PostgreSQL does, and a query on one partition of a large table is faster (#527).
- Equivalence classes with a constant in `EXPLAIN`, as PostgreSQL makes them. A condition `x = 5` on one side of an `=` join gives each side the condition of the constant, and the join no longer has the `=` condition. The nullable side of a left join gets the condition too, two constants that are not equal give a `Result` with `One-Time Filter: false`, and the new conditions prune the partitions of a partitioned table. A full join with a condition that is not an `=` of the two sides is now a hash join, as in PostgreSQL (#529).

## 0.1.62 (2026-10-22)

### Added

- Typed tables. `CREATE TABLE ... OF` takes the columns of a composite type, and the statement can give a column options, a default and constraints with `WITH OPTIONS`. `ALTER TABLE ... OF` and `NOT OF` make a table typed and untyped, with the column checks of PostgreSQL, and `pg_class` shows the type in `reloftype`. `ALTER TYPE ... ADD ATTRIBUTE`, `DROP ATTRIBUTE`, `ALTER ATTRIBUTE ... TYPE` and `RENAME ATTRIBUTE` change the typed tables with `CASCADE` and give the error of PostgreSQL without it. A typed table cannot get a column, lose a column, change the type of a column, rename a column, inherit or become a partition, with the errors of PostgreSQL (#525).
- Materialized views. `CREATE MATERIALIZED VIEW` keeps the rows of its query, also with a list of column names and `WITH NO DATA`. `REFRESH MATERIALIZED VIEW` makes the rows again, also with `WITH NO DATA`, and a scan of a materialized view with no data gives the error of PostgreSQL. A materialized view can have indexes and comments, `COPY ... TO`, `ALTER MATERIALIZED VIEW` and `DROP MATERIALIZED VIEW` work, and `pg_class`, `pg_rewrite` and `pg_matviews` show it. `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`, `COPY ... FROM` and `FOR UPDATE` give the errors of PostgreSQL. `CONCURRENTLY` has the checks of PostgreSQL and then refreshes as the statement without it does, and only the owner can refresh (#524).
- `ALTER TABLE ... INHERIT` and `NO INHERIT`. The child must have each column and each inherited constraint of the new parent already, with the errors of PostgreSQL, and its columns and constraints then count the parent and stay local. `NO INHERIT` keeps them, and they become local when they have no other parent. `pg_inherits` keeps the numbers of the other parents. `CREATE TABLE ... INHERITS` no longer gives `moving and merging` for a column that merges in its own place after another column that merges (#523).
- `ALTER TABLE` of a table with children of `INHERITS`. `ADD COLUMN` goes on to the children, and a child with a column of the same name merges it, with the notice and the type errors of PostgreSQL. `DROP COLUMN`, `ALTER COLUMN TYPE`, `SET DEFAULT`, `SET NOT NULL`, `ADD CONSTRAINT`, `DROP CONSTRAINT`, `RENAME COLUMN` and `RENAME CONSTRAINT` go on to the children and keep `attislocal`, `attinhcount`, `conislocal` and `coninhcount` as PostgreSQL does, with the errors of `ONLY` and of an inherited column or constraint. `SET NOT NULL` with `ONLY` gives a `NO INHERIT` constraint, and an identity stays on the parent (#522).
- `MERGE` into a table with children of `INHERITS`. An update or a delete of a row of a child changes the row in its table, with the triggers and the check constraints of the child, and an insert goes to the table of the statement. `ONLY` merges into the rows of the parent alone (#521).
- `pg_get_partition_constraintdef` shows the partition constraint of a partition, as PostgreSQL makes it from the bounds of the partition and of the partitions above it. `\d+` of `psql` shows it as the partition constraint. A constant of a key with a collation that is not the collation of its column does not show `COLLATE` yet (#520).
- `CREATE TABLE ... INHERITS`. The new table takes the columns, the defaults, the check constraints and the not-null constraints of its parents, and the columns of two parents or of the statement with the same name merge, with the notices and the errors of PostgreSQL. A `SELECT` of a parent gives the rows of its children too, `ONLY` gives the rows of the parent alone, and `tableoid` gives the table of each row. `TRUNCATE` of a parent truncates its children, `DROP TABLE` of a parent needs `CASCADE`, and `pg_inherits` and `relhassubclass` show the parents (#518).
- `UPDATE` and `DELETE` of a table with children of `INHERITS`. The statement changes the rows of the children too, and `ONLY` changes the rows of the parent alone. The row triggers of each child fire for its rows, the check constraints of each child apply, and `RETURNING tableoid` gives the table of each row (#519).

### Fixed

- `ALTER COLUMN TYPE` of a column that `ADD COLUMN` added to a table with rows no longer gives `a function got a value of the wrong kind`. A row from before the new column now gives null for it (#525).

## 0.1.61 (2026-10-21)

### Added

- Foreign keys of partitioned tables and foreign keys that refer to partitioned tables (#517). Each partition gets a clone of the foreign key, also a new partition and a partition of `ATTACH PARTITION`, which keeps a foreign key of its own that is the same. A foreign key that refers to a partitioned table gets a clone for each partition of that table, with the names `_1`, `_2` and so on. The errors of the checks name the leaf partition, a row that moves to another partition fires the foreign keys of the partitioned table, and `DETACH PARTITION` checks that no row refers to a key of the partition. `MERGE` and `INSERT ... ON CONFLICT` on a partitioned table with foreign keys give `0A000` for now.
- `MERGE` into a partitioned table and into a partition. Each action goes to the leaf partitions: an update can move a row to another leaf, an insert goes to the leaf that takes the row, and the triggers of the leaves fire in the order of the join. A row that moves is in the transition tables of `DELETE` and of `INSERT`, as in PostgreSQL. `RETURNING` can read `tableoid` (#510).
- `pg_identify_object(oid, oid, integer)`. It gives the type, the schema, the name and the identity of an object, as in PostgreSQL. The identity always has the schema, and an object that does not exist gives only its type. Liquibase calls this function to read the dependencies of the objects of a schema (#512).
- Partitions whose columns are in another order than the columns of the parent table, or that have dropped columns. `CREATE TABLE ... PARTITION OF` a table with a dropped column and `ATTACH PARTITION` of such a table now work. The check constraints, the not-null constraints, the indexes and the conditions of the triggers that a partition takes from its parent use the numbers of the columns of the partition. `RENAME COLUMN` of a partitioned table renames the column in its partitions too (#515).
- The counts of the rows that the transactions write, in `pg_stat_user_tables`, `pg_stat_all_tables` and `pg_stat_xact_user_tables`: `n_tup_ins`, `n_tup_upd`, `n_tup_del`, `n_live_tup`, `n_dead_tup`, `n_mod_since_analyze` and `n_ins_since_vacuum`. A rollback, a rollback to a savepoint and `TRUNCATE` change the counts as in PostgreSQL. Before, each count was 0, so a tool that reads `n_live_tup` to estimate the size of a table, such as Metabase, got 0. `n_tup_hot_upd` and `n_tup_newpage_upd` stay 0 (#514).

### Fixed

- A comparison of an `oid` with an `int2`, such as `oid = 25::int2` or `oid = ANY($1)` with an `int2[]` parameter, now gives the result of PostgreSQL. Before, it gave the error `XX000` "a function got a value of the wrong kind", because the cast function `int4(smallint)` gave its value as an `int8`. pgAdmin 4 sends these queries (#509).
- An error of a constraint, of a domain or of a partition now has the names of its object in the fields `s`, `t`, `c`, `d` and `n` of `ErrorResponse`, as PostgreSQL gives them: the schema, the table, the column, the type and the constraint. A unique violation, a foreign key violation, a check violation, a not-null violation, a domain error and a row with no partition have them, also in `ALTER TABLE`, `CREATE UNIQUE INDEX` and `COPY FROM`. `RAISE ... USING SCHEMA`, `TABLE`, `COLUMN`, `DATATYPE` and `CONSTRAINT` set them, and `GET STACKED DIAGNOSTICS` reads them. Clients such as Active Record, Ecto, Prisma and npgsql read the name of the constraint from these fields (#511).
- The rows of `pg_attribute` for a table now come in the order of PostgreSQL when a query has no `ORDER BY`: the system columns from `tableoid` to `ctid`, then the user columns. Before, the system columns came from `ctid` to `tableoid`. Sequelize reads the columns of an index with `array_agg` in this order (#513).

## 0.1.60 (2026-10-21)

### Added

- `COPY FROM` and `INSERT ... ON CONFLICT` on a partitioned table with triggers. The row triggers of the partition of each row fire, and the statement triggers and the transition tables of the partitioned table get the rows of the partitions. `EXCLUDED` is the row that the triggers `BEFORE INSERT` give, and `DO UPDATE` fires the triggers `BEFORE UPDATE` of the partition. A trigger of a partition that puts a row out of the bounds of the partition gives the error of PostgreSQL (#504).
- Row triggers on an `UPDATE` that moves a row to another partition. The triggers `BEFORE UPDATE` of the old partition fire, then `BEFORE DELETE` of the old partition and `BEFORE INSERT` of the new partition, and then `AFTER DELETE` and `AFTER INSERT`, as in PostgreSQL. Transition tables of `UPDATE` and `DELETE` on a partitioned table, with the rows that move (#508).

### Fixed

- The workflow `fuzz` now builds for `x86_64-unknown-linux-gnu`. Before, each job failed after a few seconds, because cargo-fuzz used the musl target of its own binary and the nightly toolchain does not have that target.
- `SHOW rupg.version`, `current_setting('rupg.version')` and the other reads of a parameter give the version of rupg, as spec/05 section 5.7 says. The parameter has the context `internal`, so `SET rupg.version` gives `55P02`. Before, the server sent `rupg.version` at startup, but `SHOW rupg.version` gave `42704`. `SHOW ALL` and `pg_settings` do not show it, as for `rupg.compat_version`.
- A `SELECT` with no sort and no aggregate now sends the rows that it made before an error in its targets, and then the error, as PostgreSQL does. Before, a division by zero in the fourth row gave the error and no rows. In the extended protocol the error comes only when `Execute` asks for the row that fails, so an `Execute` with a row limit can give `PortalSuspended` before the error. The test `pipeline_abort` of `libpq_pipeline` needs this. An error in a subquery in `FROM` or in `WHERE` still gives no rows.

## 0.1.59 (2026-10-21)

### Added

- `ALTER TABLE ... ATTACH PARTITION`, `ALTER TABLE ... DETACH PARTITION` and `ALTER INDEX ... ATTACH PARTITION`. The new partition must have the columns of its parent with the same types and collations, the not-null constraints and the check constraints of its parent, and no identity column, and the errors are the errors of PostgreSQL. Its rows must be in its bound, and the rows of the default partition must not be in the new bound. Each index of the parent gets an index of the partition with the same definition, or a new index. `DETACH PARTITION` makes the columns, the constraints and the indexes of the partition local again. `CONCURRENTLY` runs in one transaction, so `FINALIZE` has no detach to complete. `ALTER INDEX ... ATTACH PARTITION` makes the index of a partition a partition of an index of its parent table, and the parent index becomes valid when each partition has a valid index (#496).
- `ALTER TABLE` of partitioned tables and of partitions. `ADD COLUMN`, `SET DEFAULT`, `DROP COLUMN`, `ALTER COLUMN ... TYPE`, `SET NOT NULL`, `DROP NOT NULL`, `SET STATISTICS`, `SET STORAGE`, `ADD CONSTRAINT` and `DROP CONSTRAINT` go on to the partitions, and the partitions get the counts of the inherited columns and constraints of PostgreSQL. A check constraint of a partition with the same name and expression merges with the new constraint. `ONLY`, a column of a partition key, and an inherited column or constraint of a partition give the errors of PostgreSQL. An identity column and a foreign key of a partitioned table are not supported yet (#497).
- A checkpoint cuts the free arenas at the end of the file, as spec/08 section 8.9.3 says. The new length goes in the slot first, and the file gets shorter after that slot is durable (#456).
- The workflow `fuzz` for the long fuzz runs of spec/21 section 21.7. It runs on request and gives each target one libFuzzer worker on each CPU of the runner. With the defaults, each of the four M2 targets gets 25.3 CPU hours (#471).
- Identity columns of partitioned tables. Each partition gets the identity of the column of its parent table, with no sequence of its own, and an `INSERT` into a partition takes the next value of the sequence of the topmost parent, as `getIdentitySequence` finds it. `ADD COLUMN`, `ADD GENERATED ... AS IDENTITY`, `SET GENERATED`, the `SET` options of the sequence and `DROP IDENTITY` go on to the partitions, and give the errors of PostgreSQL with `ONLY` and on a partition. `CREATE TABLE ... PARTITION OF` and `ATTACH PARTITION` give the identity to the new partition, and `DETACH PARTITION` takes it away (#499).
- `INSERT ... ON CONFLICT` into partitioned tables. Each row goes to its partition, and the arbiters are the indexes of the partition whose partitioned index the clause infers. `DO UPDATE` changes the row in the partition, and a new row for another partition gives the error of PostgreSQL (#502).
- Triggers on partitioned tables. A row trigger gets a clone in each partition, also in a partition that `PARTITION OF` or `ATTACH PARTITION` makes later, and `DETACH PARTITION` drops the clones. `pg_trigger.tgparentid` names the trigger of the parent. A clone cannot be dropped or renamed alone, and `ALTER TRIGGER ... RENAME` and `ALTER TABLE ... ENABLE TRIGGER` on the parent change the clones too. `INSERT`, `UPDATE` and `DELETE` on a partitioned table fire its statement triggers and the row triggers of the partition of each row. A trigger `BEFORE` that moves a new row to another partition gives the error of PostgreSQL. `COPY FROM`, `ON CONFLICT`, an `UPDATE` that moves a row with row triggers, and transition tables of `UPDATE` and `DELETE` are not supported yet (#503).
- `CREATE UNLOGGED TABLE`, `CREATE UNLOGGED SEQUENCE`, `CREATE UNLOGGED TABLE AS`, `SELECT INTO UNLOGGED`, and `ALTER TABLE` or `ALTER SEQUENCE` with `SET LOGGED` and `SET UNLOGGED`. `pg_class.relpersistence` is `u` for the table, its indexes, its TOAST table and its owned sequences. A partition can be unlogged, and `SET LOGGED` and `SET UNLOGGED` work on a partition. The errors for a temporary schema, a temporary table, a partitioned table, a materialized view and a foreign key between a permanent table and an unlogged table are the errors of PostgreSQL. The rows of an unlogged table stay in memory as for a permanent table. With this change, the `libpq_pipeline` tests `pipeline_abort` and `pipelined_insert` can run.

### Fixed

- When one `ALTER TABLE` adds a column and then sets the default of the column, the rows of the table get the default of `ADD COLUMN`, as in PostgreSQL. Before, they got the new default (#498).

## 0.1.58 (2026-10-21)

### Added

- `CREATE TABLE ... PARTITION BY` and `CREATE TABLE ... PARTITION OF`. A partitioned table gets its partition key with columns, expressions, collations and operator classes, and a partition gets its bound `FOR VALUES IN`, `FOR VALUES FROM ... TO`, `FOR VALUES WITH` or `DEFAULT`. The bounds are checked against the bounds of the other partitions, and the errors are the errors of PostgreSQL. A partition gets the columns, the defaults, the check constraints and the not-null constraints of its parent. `pg_partitioned_table`, `pg_inherits`, `relpartbound`, `pg_get_partkeydef()` and `pg_get_expr()` of a bound show the new tables. Queries and writes on partitioned tables are not supported yet (#490).

- `INSERT` into a partitioned table and into a partition, and `SELECT` from a partitioned table. Each row goes to the leaf partition that its range, list or hash key takes, through the sub-partitions and the default partitions, and the constraints of the leaf check it. A row that no partition takes gives `23514` with the values of its key. `ONLY` gives no rows, and `tableoid` gives the leaf of each row. `MERGE`, `COPY` and triggers on partitioned tables are not supported yet (#491).

- `UPDATE` and `DELETE` of a partitioned table and of a partition. Each partition changes its rows, and a new row that its partition does not take moves to the partition that its key takes, as `ExecCrossPartitionUpdate` moves it. `RETURNING` gives the partition of each row in `tableoid`. An `UPDATE` of a partition checks the partition constraint first (#492).

- `COPY FROM` into a partitioned table, `COPY TO` of a partitioned table and `TRUNCATE` of a partitioned table. `COPY FROM` routes each row to its leaf partition, and an error has the line of its row. `COPY TO` sends the rows of each partition in the order of `find_all_inheritors`, as PostgreSQL 19 does. `TRUNCATE` empties the partitions too, and `TRUNCATE ONLY` of a partitioned table gives `42809` (#493).

- Indexes, unique indexes and primary keys of partitioned tables. `CREATE INDEX` on a partitioned table makes a partitioned index, and each partition gets an index: an index of the partition with the same definition that has no parent index yet, or a new index. `CREATE INDEX ON ONLY` makes an index that is not valid when the table has partitions. A new partition gets an index for each index of its parent table. A unique index or a primary key must include all the columns of the partition key, and the errors are the errors of PostgreSQL. `INSERT`, `UPDATE` and `COPY FROM` check the unique keys in each partition. `DROP INDEX` of the index of a partition gives `2BP01`, and `DROP TABLE` of a partition drops its indexes. The catalogs show `relkind` `I`, `pg_inherits`, `relispartition`, `indisvalid` and `conparentid` (#494).

## 0.1.57 (2026-10-21)

### Added

- Triggers on `MERGE`. The triggers `FOR EACH STATEMENT` of the actions of the clauses fire, the triggers `BEFORE ... FOR EACH ROW` can change or skip the row of an action before the checks, and the triggers `AFTER ... FOR EACH ROW` and the transition tables get the rows of each action (#487).
- `CREATE CONSTRAINT TRIGGER`. The trigger gets a constraint of the kind `t` with its name, and `DROP TRIGGER` drops the constraint. A trigger `DEFERRABLE` and `INITIALLY DEFERRED` fires at the commit, and `SET CONSTRAINTS` changes when it fires. A rollback to a savepoint gives back the deferred events of the savepoint (#486).
- `pg_trigger_depth()`. It gives the number of the trigger functions that run, one in the other, and 0 outside of a trigger (#485).
- Triggers on `COPY FROM`. The triggers `INSERT ... FOR EACH STATEMENT` fire before the first row and at the end. A table with triggers `BEFORE ... FOR EACH ROW` gets one row at a time, after the condition `WHERE` and before the checks, and the messages of such a trigger have the line of the copy in their context. The triggers `AFTER ... FOR EACH ROW` and the transition tables get the rows of the copy (#484).
- Triggers on `INSERT ... ON CONFLICT`. The statement fires its triggers `INSERT`, and with `DO UPDATE` also its triggers `UPDATE`. The triggers `BEFORE ... FOR EACH ROW` fire for each row in turn, so a trigger can change or skip a row before the conflict check, and the triggers `UPDATE` can change or skip the new row of `DO UPDATE`. The transition tables have the rows that went in and the rows that changed (#483).
- `r.*` of a variable in PL/pgSQL. A query in a function can now use `r.*` and `row(r.*)` for a variable of a row type, and `OLD` and `NEW` in a trigger have the row type of the table, so `row(OLD.*)`, `OLD.*::text` and `OLD IS DISTINCT FROM NEW` work. The context line of an error or a notice now shows the name of a table row type in the arguments of the function (#482).
- Triggers `INSTEAD OF ... FOR EACH ROW` on views. An `INSERT`, an `UPDATE` and a `DELETE` of a view with such a trigger now call the trigger for each row, with the triggers `FOR EACH STATEMENT` of the view before and after them. `RETURNING` gives the row that the trigger returns (#480).
- Transition tables of triggers, as in `REFERENCING OLD TABLE AS o NEW TABLE AS n`. An `INSERT`, an `UPDATE` and a `DELETE` on a table with such a trigger now run, and the queries of the trigger function read the old and the new rows of the statement by the names of the tables, also in `EXECUTE`. A write into a transition table gives the error of PostgreSQL (#479).
- Utility statements in PL/pgSQL, as in `CREATE TABLE`, `ALTER`, `DROP`, `CREATE TABLE AS`, `TRUNCATE` and `SET`, both as statements and with `EXECUTE`. The next statements of the call see the changes of the catalog, and a plan made before a change is made again. A block with `EXCEPTION` undoes the changes of the catalog and the settings of the statements in the block. `GET DIAGNOSTICS` gives the number of rows of `CREATE TABLE AS`. The errors of `INTO`, of the transaction commands, of `COPY` to the client and of `VACUUM` are the errors of PostgreSQL (#478).

### Security

- `rustls-webpki` is now 0.103.16, which fixes RUSTSEC-2026-0382. The old version can accept a certificate path that has a distrusted emSign intermediate (#475).

### Changed

- The lowest allowed versions of `libc`, `ring`, `rustls` and `loom` are now the versions in `Cargo.lock`. The old bounds let Cargo pick `libc` 0.2.0, which `ring` does not accept (#455).

### Fixed

- A whole-row `old` or `new` in `RETURNING` of `MERGE` gives the row. Before, it gave one column of the row. The old row is null for an insert, and the new row is null for a delete (#488).
- A new range type no longer loses memory. The rows of the type were copied from four boxes that were not freed (#455).

## 0.1.56 (2026-10-21)

### Added

- The assignment to an element or a slice of an array in PL/pgSQL, as in `x[2][1] := 30` and `x[2:3] := ARRAY[5, 6]`, for arrays of more than one dimension. An assignment to an element or a slice out of the bounds of a one-dimensional array makes the array larger, and the errors of the subscripts and of the bounds are the errors of PostgreSQL (#470).
- Subscripts on a column in `UPDATE`, `INSERT`, `ON CONFLICT DO UPDATE` and `MERGE`, as in `UPDATE t SET a[2] = 5` and `INSERT INTO t (a[1], a[3]) VALUES (1, 3)`. More than one item can set parts of the same column, and a domain over an array checks the new value after all of the items (#472).
- The assignment to a field of a composite value, as in `UPDATE t SET p.x = 5`, `INSERT INTO t (p.x, p.y) VALUES (1, 2)` and `x.a.x := 1` in PL/pgSQL. Fields and subscripts can be mixed, as in `UPDATE t SET ps[1].x = 7` (#474).
- `COMMIT` and `ROLLBACK` in PL/pgSQL, with `AND CHAIN`. A procedure that `CALL` runs outside of a transaction block, and the code of such a `DO`, can end the transaction, and a new transaction starts at once. A loop over a query or over a cursor goes on after the end. The errors in a function, in a trigger, in a transaction block and in a block with `EXCEPTION` are the errors of PostgreSQL (#476).

## 0.1.55 (2026-10-21)

### Added

- `CREATE PROCEDURE` and `CALL` with input, `INOUT` and `OUT` arguments, named arguments, defaults and `VARIADIC`. A procedure with output arguments gives one row. `DROP PROCEDURE`, `DROP ROUTINE`, `ALTER PROCEDURE`, `COMMENT ON PROCEDURE` and `CREATE OR REPLACE` check the kind of the routine as PostgreSQL does. (#464).
- `CALL` in PL/pgSQL and in the extended protocol. In PL/pgSQL, the values of the `INOUT` and `OUT` arguments go back to the variables of the call, and an argument that is not a variable gives the error of PostgreSQL (#465).
- `FOREACH ... SLICE` in PL/pgSQL, with the checks of PostgreSQL for the slice and for the type of the loop variable (#466).
- A field of a record as a target of `INTO`, of `GET DIAGNOSTICS` and of the loops in PL/pgSQL (#467).
- Cursors in PL/pgSQL: bound and unbound cursor variables, `OPEN` with arguments, `OPEN FOR EXECUTE`, `FETCH`, `MOVE`, `CLOSE` and the `FOR` loop over a cursor. A cursor that a function opens is a portal of the session, so SQL can fetch from a `refcursor` that a function gives (#468).

### Fixed

- A syntax error in the code of `DO` has its place in the query, as in PostgreSQL. Before, the place was in the code (#466).

## 0.1.54 (2026-10-20)

### Added

- Functions in PL/pgSQL and the statement `DO`. A block with `EXCEPTION` runs in a subtransaction, and the errors and the messages have the context lines of PostgreSQL. `CALL` and `DO` inside PL/pgSQL, `FOREACH` with `SLICE` and triggers in PL/pgSQL are not supported yet (#458).
- `%TYPE` in the signatures of functions. `CREATE FUNCTION` checks a PL/pgSQL body, and the assignments in PL/pgSQL coerce their values as PostgreSQL does (#459).
- `CREATE TRIGGER`, `DROP TRIGGER`, `ALTER TRIGGER ... RENAME`, `ALTER TABLE ... ENABLE/DISABLE TRIGGER` and `COMMENT ON TRIGGER`, with `pg_trigger` and `pg_get_triggerdef`. The triggers do not fire yet, and a write to a table with a user trigger that fires gives an error (#460).
- The triggers of `CREATE TRIGGER` fire on `INSERT`, `UPDATE` and `DELETE` with a trigger function in PL/pgSQL, with `NEW`, `OLD`, the variables `TG_*`, `WHEN` and `UPDATE OF`. The notices of a statement with no rows now go to the client (#461).
- `DO` runs in a PL/pgSQL function and in a `DO` block, with the statement as the context of the messages and the errors of its code. `DO LANGUAGE sql` gives 0A000, as in PostgreSQL (#462).
- `SET ROLE`, `SET SESSION AUTHORIZATION` and `RESET` of them change `current_user`, `session_user` and `is_superuser`, with the checks of PostgreSQL. A new object belongs to the current user (#453).

### Changed

- The facade takes a checkpoint when all the log rings together hold more than `Options::max_wal_size` bytes, default 1 GiB, as `max_wal_size` does in PostgreSQL. `Options::checkpoint_log` is now the limit of one ring (#454).

### Fixed

- The workspace builds again with Rust 1.88, the lowest supported version. Four match arms used `if let` guards, which need a newer Rust (#457).

## 0.1.53 (2026-10-20)

### Added

- `CREATE ROLE`, `ALTER ROLE`, `DROP ROLE`, `ALTER ROLE ... RENAME TO`, and `GRANT` and `REVOKE` of a role, with the errors and notices of PostgreSQL (#451).
- Temporary tables, views and sequences in the schema `pg_temp_N` of the session, which goes when the session ends (#449).

### Fixed

- A join that makes more rows than rupg can keep in memory gives the error `out of memory` and the server does not stop. The cross join of the regression test `groupingsets` stopped the server before (#448).
- A recursive query of `WITH` with no end under a `LIMIT` gives the rows of the `LIMIT`, and the server does not stop (#450).

## 0.1.52 (2026-10-20)

### Added

- `EXPLAIN` in the text, JSON, XML and YAML forms, with a planner that makes the plan of a query with the costs of PostgreSQL (#439).
- `ALTER DATABASE ... SET` and `RESET`, with the settings applied at the start of each session of the database and shown in `pg_db_role_setting` (#440).
- `COPY ... FROM 'file'`, which reads a file of the server, for a superuser (#441).
- `VACUUM` and `ANALYZE`, which check their options, relations and columns and then end, because rupg has no statistics yet (#442).
- `CREATE TABLE AS` and `SELECT ... INTO`, which make a table from the columns of a query and fill it with the rows of the query (#443).

### Fixed

- A `LATERAL` relation in a nested loop no longer keeps the rows of every run until the query ends. This stopped the server with no memory in the regression tests `join` and `subselect` (#444).
- A correlated subquery finds its rows through an index of a column for `=` with an expression of the outer columns, not only with an outer column (#445).
- A `LATERAL` item that reads no relation on the left side of its join runs once, and not once for each row of the left side (#446).
- A `LATERAL` part of a join that reads only relations outside the join runs once, not once for each row of the left side (#446).

## 0.1.51 (2026-10-20)

### Added

- `ALTER FUNCTION` and `ALTER ROUTINE` with the options of a function, `RENAME TO`, `SET SCHEMA` and `OWNER TO`, and `COMMENT ON FUNCTION` (#437).
- `VARIADIC` arguments for functions in SQL, with the checks of PostgreSQL for the names and types of the arguments, and functions in SQL that return `void` (#436).
- The defaults of the arguments of functions in SQL, with `DEFAULT` or `=`. A call can leave out the last arguments or name its arguments, and `pg_get_function_arguments` shows the defaults (#435).
- `CREATE OR REPLACE FUNCTION` for functions in SQL. The function keeps its OID and owner, and the checks of PostgreSQL for the return type, the `OUT` parameters and the names of the arguments apply (#434).
- `CREATE FUNCTION` and `DROP FUNCTION` for functions in SQL of a user. A body can be a `RETURN` expression, `BEGIN ATOMIC` or a string, and it can name the arguments. A function can return a set, a composite type or `OUT` parameters, and the dependencies of a function work with `DROP ... CASCADE` (#433).
- `pg_describe_object`, the description of an object for each kind of object of the catalog. A name has its schema when the search path does not find it, and an object that does not exist gives null (#432).

## 0.1.50 (2026-10-20)

### Added

- `ALTER TYPE` and `ALTER DOMAIN` with `RENAME TO`, `SET SCHEMA` and `OWNER TO`. The array type changes with its type, a composite type changes with its relation, a domain moves with its constraints, and the multirange type of a range type gets the new owner. The views that name the type get a new text (#428).
- `ALTER TYPE` with `ADD ATTRIBUTE`, `DROP ATTRIBUTE`, `ALTER ATTRIBUTE ... TYPE` and `RENAME ATTRIBUTE` for composite types. Stored values get a null for a new attribute and lose a dropped one, also in arrays, domains and other composite types. A change of the type of an attribute that a table column holds gives the error of PostgreSQL (#429).
- Views, defaults, check constraints, indexes and domain checks depend on the fields of composite types that they select. `DROP ATTRIBUTE` and `DROP COLUMN` of a row type stop with an error or drop these objects with `CASCADE`, and a view shows the new name of a field after `RENAME ATTRIBUTE` (#430).

## 0.1.49 (2026-10-20)

### Added

- `ALTER TYPE ... ADD VALUE` and `ALTER TYPE ... RENAME VALUE` for enum types, with the sort order and the OID of PostgreSQL, the order of labels with odd OIDs, and the error for a new label that the transaction uses before the commit (#426).
- `ALTER TABLE`, `ALTER VIEW` and `ALTER SEQUENCE` with `SET SCHEMA`. The relation moves with its row type, its indexes, its owned sequences and its constraints, and the views that read it get a new text (#425).
- `ALTER TABLE ... CLUSTER ON`, `SET WITHOUT CLUSTER` and `REPLICA IDENTITY`. The catalog keeps `indisclustered`, `indisreplident` and `relreplident`, and `DROP NOT NULL` keeps the not-null constraint of a key column of the replica identity index (#424).
- The options of relations: `WITH (...)` of `CREATE TABLE`, `CREATE INDEX`, `CREATE VIEW` and the index of a constraint, and `SET (...)` and `RESET (...)` of `ALTER TABLE`, `ALTER INDEX` and `ALTER VIEW`. The options of the namespace `toast` go to the TOAST table. `ALTER TABLE ... SET WITHOUT OIDS` is accepted (#423).
- `ALTER TABLE ... ALTER COLUMN ... SET STATISTICS`, `SET STORAGE`, `SET COMPRESSION`, and `SET` and `RESET` of the column options, with `STORAGE` and `COMPRESSION` in `CREATE TABLE`. The catalog keeps `attstattarget`, `attstorage`, `attcompression` and `attoptions` (#422).

## 0.1.48 (2026-10-20)

### Added

- `ALTER TABLE ... DROP COLUMN` with `IF EXISTS` and `CASCADE`. The column stays in `pg_attribute` with `attisdropped`, so the other columns keep their numbers, and the indexes, constraints, defaults and sequences on the column go with it (#420).
- `ALTER ... RENAME` of a table, a view, an index, a sequence, a column, a constraint of a table and a schema. The row type, the array type and the index of a key follow the new name, and the views that read a renamed object keep working (#419).
- `ALTER TABLE ... DROP CONSTRAINT` with `IF EXISTS` and `CASCADE`, and `ALTER COLUMN ... DROP NOT NULL`. A primary key or a unique constraint takes its index with it, and the objects that depend on the constraint give the PostgreSQL error or go with `CASCADE` (#418).
- `ALTER TABLE ... ALTER COLUMN ... TYPE` with `USING` and `COLLATE`. The statement changes the default, the indexes, the check constraints and the foreign keys on the column, writes the rows again with the new type, and gives the PostgreSQL errors for a view or a rule that uses the column (#417).

## 0.1.47 (2026-10-19)

### Added

- `ALTER TABLE ... ADD COLUMN` and `ALTER COLUMN ... SET NOT NULL`. A statement that changes a table with rows now changes and checks the rows, so `ADD COLUMN`, `ADD CONSTRAINT`, `SET NOT NULL` and `CREATE UNIQUE INDEX` work on a table with rows (#415).
- `COPY ... FROM STDIN` reads rows with `DEFAULT`, `ON_ERROR IGNORE`, `ON_ERROR SET_NULL`, `REJECT_LIMIT` and `LOG_VERBOSITY`, and the input of a field of a domain checks the constraints of the domain (#414).
- `COPY ... FROM STDIN` reads rows in the text, CSV and binary formats, with the defaults of the other columns, `FORCE_NULL`, `FORCE_NOT_NULL`, the condition `WHERE`, the constraints and the foreign keys of the table (#413).

## 0.1.46 (2026-10-19)

### Added

- `SAVEPOINT`, `RELEASE SAVEPOINT` and `ROLLBACK TO SAVEPOINT`. A rollback to a savepoint brings back the rows, the catalog, the settings, the deferred triggers and the advisory locks of the transaction at the savepoint (#411).
- `TRUNCATE` with `RESTART IDENTITY`, `CONTINUE IDENTITY`, `CASCADE` and `RESTRICT`. A rollback brings the rows back (#410).
- `ALTER TABLE ... ALTER COLUMN` with `ADD GENERATED ... AS IDENTITY`, `SET GENERATED`, the `SET` options of the identity sequence and `DROP IDENTITY [IF EXISTS]` (#409).
- The identity columns: a column can be `GENERATED ALWAYS AS IDENTITY` or `GENERATED BY DEFAULT AS IDENTITY` with the options of its sequence, and `INSERT` and `MERGE` accept `OVERRIDING SYSTEM VALUE` and `OVERRIDING USER VALUE` (#408).

## 0.1.45 (2026-10-19)

### Added

- The statements that change data in `WITH`: an `INSERT`, an `UPDATE`, a `DELETE` or a `MERGE` in `WITH` runs in the command of the statement, and the other parts can read the rows of its `RETURNING` (#406).
- `WITH` before `INSERT`, `UPDATE`, `DELETE` and `MERGE`: the queries of `WITH` can be read in all the parts of the statement (#405).
- The deferred foreign keys: a foreign key that is `DEFERRABLE INITIALLY DEFERRED` checks its rows at the commit, and `SET CONSTRAINTS` with `ALL` or a list of names makes the constraints of a transaction `DEFERRED` or `IMMEDIATE` (#404).

## 0.1.44 (2026-10-19)

A patch release on the way to M3. `INSERT ... ON CONFLICT` and `MERGE` now run, and the foreign keys run their actions.

### Added

- `MERGE` with `WHEN MATCHED`, `WHEN NOT MATCHED BY SOURCE` and `WHEN NOT MATCHED BY TARGET`, the actions `INSERT`, `UPDATE`, `DELETE` and `DO NOTHING`, and `RETURNING` with `merge_action()`, `old` and `new` (#402).
- `INSERT ... ON CONFLICT DO NOTHING` and `INSERT ... ON CONFLICT DO UPDATE`, with a list of columns or `ON CONSTRAINT` as the inference, `excluded`, `WHERE` and `RETURNING` with `old` and `new` (#401).
- The actions `CASCADE`, `SET NULL` and `SET DEFAULT` of the foreign keys for `DELETE` and `UPDATE` of the referenced table. Each action runs the same statement as PostgreSQL on the referencing table, and the triggers of its rows fire in the same order as in PostgreSQL (#400).

## 0.1.43 (2026-10-19)

A patch release on the way to M3. Writes now check the unique indexes, the primary keys and the foreign keys, and `RETURNING` gives the rows that a write changed.

### Added

- `INSERT` and `UPDATE` check the unique indexes and the primary keys of the table. A duplicate key gives `23505` with the key in the detail, and a key that a transaction in progress holds makes the write wait for the end of that transaction (#395).
- `RETURNING` for `INSERT`, `UPDATE` and `DELETE`, with the names `old` and `new` and the option `RETURNING WITH (OLD AS o, NEW AS n)`. The list can read the columns of `FROM` and `USING` and can have subqueries, and the session sends the rows in the simple and the extended protocol (#396).
- `INSERT`, `UPDATE` and `DELETE` check the foreign keys after the statement, with `MATCH SIMPLE`, `MATCH FULL`, `NO ACTION` and `RESTRICT`. The errors and their details are the same as in PostgreSQL (#397).

## 0.1.42 (2026-10-18)

A patch release on the way to M3. Tables now take rows: `INSERT`, `UPDATE` and `DELETE` work on the rows in memory, with MVCC snapshots and the waits for the rows that other transactions change.

### Added

- `INSERT` with `VALUES`, with a query and with `DEFAULT VALUES` into a table, with the defaults of the columns and of the domains, the not-null and check constraints, and the errors of PostgreSQL. The rows are in memory with MVCC snapshots, so a rollback removes them and other sessions see them only after the commit (#391).
- `UPDATE` and `DELETE` on a table, with `SET` from `DEFAULT` and `ROW(...)`, `FROM` and `USING`, the check and not-null constraints, and the errors of PostgreSQL. A change of a row that another transaction changed waits for that transaction, with the deadlock check, `lock_timeout`, the second run in read committed and the serialization error in repeatable read (#392).

## 0.1.41 (2026-10-18)

A patch release on the way to M2. The connection rate targets of spec/06 section 6.18 now hold for the server CPU for each connection.

### Added

- `rupg serve` without `--listen` listens on each host of `listen_addresses` with the port of `port` after it reads `postgresql.conf`, as the postmaster does. `pg_settings` and `pg_file_settings` show these settings from the file (#387).

### Changed

- A connection runs on a thread that waits for the next task, so the server does not start a new thread and a new stack for each connection. The TCP stream and the Unix stream send the records of the TLS handshake with one `writev`. On server1 the server CPU for each connection with `trust` over a Unix socket is 135 µs against 9298 µs for PostgreSQL, and with SCRAM over TLS and an ECDSA P-256 certificate it is 775 µs against 11424 µs (#388).
- `boot_val` of `server_version` in `pg_settings` is the version of rupg, as `PG_VERSION` is in PostgreSQL (#387).
- Section 6.18 of spec/06 tells how the connection rate is measured when the client and the server share a machine, and which certificate the TLS case uses (#389).

## 0.1.40 (2026-10-18)

A patch release on the way to M2. The L2 comparison of the gate schema now passes for 201 of 209 catalogs and views.

### Added

- `CREATE RULE` and `DROP RULE`, with the rules in `pg_rewrite` and `pg_get_ruledef` (#381).
- `pg_import_system_collations`, which makes a collation for each locale of the C library and of ICU, as initdb does (#382).
- The settings `tcp_keepalives_idle`, `tcp_keepalives_interval`, `tcp_keepalives_count` and `tcp_user_timeout` show and set the options of the socket, and `num_os_semaphores` has the value of PostgreSQL (#383).
- The count of the sequential scans of the tables in `seq_scan` and `last_seq_scan` of `pg_stat_user_tables`, and `pg_stat_force_next_flush` (#385).

### Changed

- Section 5.2 of the specification names `pg_config` and the shared memory sizes in the exclusions (#384).

## 0.1.39 (2026-10-17)

A patch release on the way to M2. The releases from 0.1.29 to 0.1.38 have their notes on the release page of GitHub. This release brings the version of the crates up to the tag.

### Added

- `TABLESAMPLE` with the methods `BERNOULLI` and `SYSTEM` (#377).
- The stored secret of the superuser in `pg_authid`, `pg_shadow` and `pg_roles` (#378).
- `rupg serve -D DIR` reads `postgresql.conf`, `postgresql.auto.conf` and the files that they include. `pg_file_settings` shows their lines, and `pg_settings` shows `sourcefile` and `sourceline` (#379).

## 0.1.28 (2026-10-16)

A patch release on the way to M2. rupg now has full text search with the default parser, the Snowball dictionaries and the functions of PostgreSQL 19 for `tsvector` and `tsquery`.

### Added

- The Snowball dictionaries, the statements `CREATE TEXT SEARCH DICTIONARY`, `CREATE TEXT SEARCH CONFIGURATION` and `ALTER TEXT SEARCH CONFIGURATION`, and `ts_lexize` (#343).
- The default text search parser, `ts_parse`, `ts_token_type` and `to_tsvector` (#344).
- `to_tsquery`, `plainto_tsquery`, `phraseto_tsquery` and `websearch_to_tsquery` (#345).
- The `json` and `jsonb` forms of `to_tsvector`, `json_to_tsvector`, `jsonb_to_tsvector`, `ts_rank` and `ts_rank_cd` (#346).
- `ts_rewrite` with a target and a substitute, and `ts_headline` for `text` (#347).
- The `json` and `jsonb` forms of `ts_headline` (#348).

## 0.1.27 (2026-10-16)

A patch release on the way to M2. rupg now has most of the built-in functions of the date and time, numeric, text, bytea, array, json and `jsonb` types, the aggregates of PostgreSQL 19, window functions with frames, and the executor of `jsonpath`.

### Added

- The row of `plpgsql` in `pg_available_extensions` (#308).
- The internal triggers of the foreign keys in `pg_trigger` (#309).
- The functions of `system_functions.sql` run from their body in SQL, as in PostgreSQL (#310, #311).
- The kernels of the date and time arithmetic, `EXTRACT`, `date_part`, `date_trunc`, `date_bin`, `AT TIME ZONE` and `AT LOCAL` (#312, #313, #314).
- The math functions of numeric (#315).
- `make_date`, `make_timestamp`, `generate_series` of timestamps and the comparisons across the date types (#316).
- `random`, `setseed`, `gen_random_uuid`, `uuidv7` and the other random and UUID functions (#317).
- `array_position`, `array_fill`, `array_sort`, `trim_array`, the containment operators and the other array functions (#318).
- The trigonometric, hyperbolic, error and gamma functions of `float8` (#319).
- The `bytea` functions: `substring`, `overlay`, `position`, the bytes and the bits, `LIKE`, `trim`, the hashes and the CRC values (#320).
- `translate`, `overlay`, `to_bin`, `to_oct`, `unistr`, `casefold`, and `gcd` and `lcm` of the integer types (#321).
- The regression aggregates of `float8`, and `var_pop`, `var_samp`, `stddev_pop` and `stddev_samp` of the integer types and numeric (#322, #323).
- The operators and the functions that read json and `jsonb` values, the functions that change `jsonb` values, the set functions of json and `jsonb`, and the functions that make json and `jsonb` values (#324, #325, #326).
- `json_agg`, `jsonb_agg`, `json_object_agg`, `jsonb_object_agg` and their strict and unique forms (#327).
- `normalize`, `is_normalized`, `unicode_assigned`, `num_nulls`, `current_query`, `system_user` and other small functions (#328).
- `to_char` of timestamps, intervals and numbers, `to_timestamp`, `to_date` and `to_number` (#329, #330, #331).
- The functions of the size of an object, such as `pg_relation_size` and `pg_size_pretty` (#332).
- The ordered-set and hypothetical-set aggregates (#333).
- `sum` and `avg` of `interval` (#334).
- The hash support functions of the types (#335).
- The comparison of record values (#336).
- Aggregate functions with `OVER`, window functions in queries with groups, and the frames of the windows in `ROWS`, `RANGE` and `GROUPS` mode (#337, #338, #339).
- The executor of `jsonpath` and its datetime methods (#340, #341).

## 0.1.26 (2026-10-15)

A patch release on the way to M2. The `pg_dump` round trip now passes: a dump of rupg restores into PostgreSQL 19, and a dump of PostgreSQL 19 restores into rupg.

### Added

- The fields `F`, `L` and `R` in errors and notices, with the place in the source of rupg (#303).
- `ALTER TABLE ... ALTER COLUMN ... SET DEFAULT` and `DROP DEFAULT`, `ALTER TABLE ... ADD CONSTRAINT`, `OWNER TO` for tables, views, sequences and indexes, and `ALTER SCHEMA ... OWNER TO` (#304).
- `CREATE SEQUENCE`, `ALTER SEQUENCE`, `nextval`, `currval`, `lastval`, `setval` and `pg_sequence_last_value` (#305).
- `COPY FROM STDIN` with no rows, with the checks of the options, the header and the end of the data as in PostgreSQL 19. A data row gives `0A000` (#306).

### Changed

- `COPY FROM STDIN` no longer gives `0A000` at the start. It sends CopyInResponse and reads the data (#306).

## 0.1.25 (2026-10-15)

A patch release on the way to M2. Gate G1 now passes for all seven clients: psql, pgAdmin 4, pgjdbc, psycopg, node-postgres, pgx and asyncpg show the gate schema on rupg as on PostgreSQL 19. rupg also adds cursors, CREATE DATABASE, DROP, the advisory locks and the encoding conversions.

### Added

- The type `jsonpath` (#286).
- The type `xml`, with input, output, send and receive as in PostgreSQL 19 built with libxml2 (#287).
- `pg_get_indexdef` and `pg_get_constraintdef` for the indexes and the constraints of the system catalogs (#288).
- The rows of the extension `plpgsql` in `pg_extension`, `pg_language`, `pg_proc`, `pg_depend` and `pg_description`, with the OIDs of PostgreSQL 19 (#289).
- The setting `rupg.compat_version` (#290).
- CREATE DATABASE and DROP DATABASE, with `FORCE` (#291).
- `pg_sleep`, `pg_sleep_until` and the type `void`. A cancel request ends the wait (#292).
- The advisory lock functions, with a `bigint` key and with two `integer` keys (#293).
- Cursors: `DECLARE`, `FETCH`, `MOVE` and `CLOSE` in the simple and the extended protocol, and the view `pg_cursors` (#294).
- DROP TABLE, VIEW, INDEX, SEQUENCE and SCHEMA, with `CASCADE` and the messages of PostgreSQL for the objects that depend on them (#295).
- `pg_partition_tree`, `pg_partition_ancestors`, `pg_partition_root` and `pg_relation_is_publishable`, so `\d` of psql works for a table (#296).
- `pg_get_serial_sequence` (#297).
- The system columns `xmin`, `cmin`, `xmax` and `cmax`, `encode`, `decode` and the recovery control functions that pgAdmin 4 calls (#298).
- The defaults of the arguments and the calls with named arguments for the built-in functions, and the kernels of `make_interval` and `parse_ident` (#300).
- `convert`, `convert_from`, `convert_to`, `length(bytea, name)` and `to_ascii`, with the conversions between UTF8 and the 27 single-byte encodings (#301).

### Changed

- A join on an `=` of expressions is now a hash join, and `IN` or `= ANY` with a subquery that reads no column of an outer query runs the subquery one time. The queries of the clients on the catalogs are much faster (#288).

### Fixed

- A startup packet with `rupg.compat_version` is now accepted. Before, rupg refused it with 42602 (#290).

### Security

- rustls is now 0.23.46, for RUSTSEC-2026-0346, 0347, 0348 and 0349 (#299).

## 0.1.24 (2026-10-13)

A patch release on the way to M2. rupg now runs the operators and aggregates of the multirange types, and reads and writes the text search types `tsvector` and `tsquery` with their operators and the match operator `@@`.

### Added

- The operators of the multirange types, such as `+`, `-`, `*`, `&&`, `@>`, `<<` and `-|-`, and the set function `multirange_minus_multi` (#280).
- The aggregates `range_agg` and `range_intersect_agg` for ranges and multiranges (#281).
- The text and binary forms of `tsvector`, with `length`, `strip`, `setweight`, `ts_delete`, `tsvector_to_array`, `array_to_tsvector`, `ts_filter`, `unnest`, the operator `||` and the order of the vectors (#282).
- The text and binary forms of `tsquery`, with `numnode`, `querytree`, `tsquery_phrase`, the operators `&&`, `||`, `!!`, `<->`, `@>` and `<@` and the order of the queries (#283).
- The match operators `@@` and `@@@` of `tsvector` and `tsquery`, with weights, prefixes and the rules of PostgreSQL for `<N>` (#284).

### Fixed

- A message of a function with a SQLSTATE of class `00` is now a `NOTICE`, as in PostgreSQL. Before, it was a `WARNING` (#283).
- The `Bind` message now sends the notices of the input functions of the parameters. Before, the session dropped them (#283).

## 0.1.23 (2026-10-13)

A patch release on the way to M2. rupg now reads and writes `aclitem` values and the values of the range and multirange types, and runs the operators of the range types.

### Added

- The input of `aclitem`, with `makeaclitem`, `aclcontains`, `aclitemeq` and the hash functions of `aclitem`. A function can now send a warning, such as "defaulting grantor to user ID 10" (#275).
- The text and binary forms of the six built-in range types, with the constructors, the functions that read the bounds and the flags, and the order of the ranges (#276).
- The operators of the range types, `range_merge`, `range_minus_multi` and the subtype difference functions (#277).
- The text and binary forms of the six built-in multirange types, with the constructors, the cast from a range, the functions that read the bounds and the flags, `range_merge`, `unnest` and the order of the multiranges (#278).

### Fixed

- PostgreSQL sends `CopyOutResponse` before it runs the query of `COPY ... TO STDOUT`. rupg now sends it before an error of the run too (#275).
- An integer literal that fits `int4` after the parser negates it, such as `-2147483648`, is now an `int4`, as in PostgreSQL (#277).

## 0.1.22 (2026-10-13)

A patch release on the way to M2. rupg now runs the operators and functions of the geometric types, and reads and writes the values of `xid`, `cid`, `pg_lsn`, `pg_snapshot` and `txid_snapshot`.

### Added

- The operators and functions of `point`, `lseg`, `line` and `box`, such as the distance, the intersection and the closest point (#271).
- The input of `xid`, `cid` and `pg_lsn`, the text and binary forms of `pg_snapshot` and `txid_snapshot`, the `pg_lsn` arithmetic, and the functions `pg_snapshot_xmin`, `pg_snapshot_xmax`, `pg_snapshot_xip` and `pg_visible_in_snapshot` (#272).
- The operators and functions of `path`, `polygon` and `circle` (#273).

### Fixed

- `pg_get_sequence_data` gave the LSN of a sequence as the text `0/0`. It now gives the `pg_lsn` value `0/00000000`, as PostgreSQL does (#272).
- The input of `gtsvector` and of the BRIN summary types gave `0A000` with the wrong message. It now gives the message of PostgreSQL 19 (#272).

## 0.1.21 (2026-10-13)

A patch release on the way to M2. rupg now reads and writes the values of more built-in types, so that a table with columns of these types works in queries, in `COPY` and in a `pg_dump` round trip.

### Added

- The text and binary forms of `money`, `xid8`, `oid8`, `tid` and `refcursor` (#266).
- The text and binary forms of `macaddr` and `macaddr8`, with their operators, `trunc` and `macaddr8_set7bit` (#267).
- Bit string constants such as `B'1011'` and `X'1F'`, and the text and binary forms of `bit` and `bit varying`, with their operators, functions, casts and aggregates (#268).
- The text and binary forms of the geometric types `point`, `lseg`, `box`, `path`, `polygon`, `line` and `circle` (#269).

### Fixed

- PostgreSQL inlines the SQL functions `anytextcat`, `textanycat`, `quote_literal(anyelement)` and `quote_nullable(anyelement)`, and folds the call when the output function of the type is immutable. rupg did not fold these calls, so an error in the call came after the description of the rows (#268).

## 0.1.20 (2026-10-13)

A patch release on the way to M2. rupg now sends the data of a table with `COPY TO STDOUT`, so `pg_dump` can dump a database of rupg, and a nightly job does a round trip of a dump through PostgreSQL 19.

### Added

- `COPY ... TO STDOUT` for a table or a list of its columns, in the formats `text`, `csv` and `binary` (#262). `COPY` to a file or to a program gives `0A000`.
- The functions `pg_encoding_to_char` and `pg_char_to_encoding` (#263).
- The nightly round trip of `pg_dump` (#264). The job dumps a schema from rupg in the plain and custom formats, restores the dump into PostgreSQL 19, and compares the result with a dump of the same schema from PostgreSQL. See [`tests/dumprt/README.md`](tests/dumprt/README.md).

## 0.1.19 (2026-10-13)

A patch release on the way to M2. rupg now runs the queries of `WITH`, gives the definition of a view with `WITH`, and accepts the statements of `pg_dump` and `pg_restore` for the data of a sequence, for the locks and for the comments.

### Added

- `WITH` and `WITH RECURSIVE` in a query (#256), and in the definition of a view that `pg_get_viewdef` gives (#257).
- The function `pg_get_sequence_data` (#258).
- `LOCK TABLE` for the modes up to `ROW EXCLUSIVE` (#259). rupg takes no lock on a relation, so these modes run as checks only. A stronger mode gives `0A000`.
- `COMMENT ON` for the schemas, tables, views, indexes, sequences, columns, table constraints, types and domains of the user (#260). `pg_description`, `obj_description` and `col_description` show the comments.

### Fixed

- The dependencies of a view did not include the relations in a query of `WITH` that no part of the view reads (#259).

## 0.1.18 (2026-10-10)

A patch release on the way to M2. rupg now gives back the memory of an idle session, has tests for the idle size and the connection rate against PostgreSQL, and runs the replay of the catalog queries of `pg_dump` for a new database with no failed statement.

### Added

- The idle session test `cargo xtask idle` (#251). An idle session of rupg now gives back the stack of its last statement after 1 second.
- The connection rate test `cargo xtask connrate` (#252), which compares rupg with PostgreSQL for each way to connect. The client now sets `TCP_NODELAY`.
- The functions `pg_is_in_recovery`, `array_remove` and `array_replace`, the subscript of a `name`, and the rows of `pg_init_privs` for the built-in objects (#253).

### Fixed

- A correlated subquery read all the rows of its relations for each row of the outer query (#254). A scan now uses an index of the column when its condition has an `=` with a column of the outer query. The query of `pg_dump` for the functions went from 58 seconds to 0.4 seconds in a debug build.

## 0.1.17 (2026-10-10)

A patch release on the way to M2. rupg now shows its authentication files in the views of PostgreSQL, uses regular expressions in these files, and gives the privileges of the objects of the session catalog.

### Added

- The functions of the views `pg_config`, `pg_file_settings`, `pg_available_extensions` and `pg_available_extension_versions` (#245).
- The views `pg_hba_file_rules` and `pg_ident_file_mappings` (#248). They read the files again at each call and show the error of each line as PostgreSQL does.
- Regular expressions in `pg_hba.conf` and `pg_ident.conf` (#249), with `\1` in the user maps.

### Fixed

- `has_table_privilege`, `has_column_privilege`, `has_any_column_privilege` and `has_sequence_privilege` gave NULL for the system views and for the objects of the user (#246).
- `has_schema_privilege`, `has_type_privilege` and `has_function_privilege` gave an error or NULL for the objects that are not in the built-in catalogs (#247).

## 0.1.16 (2026-10-10)

A patch release on the way to M2. rupg now makes the 65 views and the 4 tables of `information_schema`, has window functions, and reads the types of extended statistics.

### Added

- The fields of a `record` value with `(x).name` (#230), and the functions `_pg_expandarray` and `_pg_index_position` of `information_schema` with a query body (#231).
- `(f(...)).*` in a target list, the comparison of two rows, and `acldefault` for a null ACL (#232).
- Window functions with `PARTITION BY`, `ORDER BY` and named windows (#233). Aggregates with `OVER` and window frames are not supported yet.
- A row on the left of `IN`, `NOT IN`, `ANY` and `ALL` with a subquery (#234).
- The views and tables of `information_schema` with the OIDs of PostgreSQL 19 (#235), and the functions that they call: `nameconcatoid` and `getdatabaseencoding` (#236), `pg_relation_is_updatable` and `pg_column_is_updatable` (#238), and `format` (#239).
- `pg_get_ruledef` and `pg_get_triggerdef` (#240), and `array_agg` of arrays (#241).
- The input and output of `pg_ndistinct` and `pg_dependencies` (#242). The view `pg_stats_ext` now runs.
- `pg_input_is_valid` and `pg_input_error_info` (#243). The input of a domain uses the typmod of the domain, and a pseudo-type gives the error of PostgreSQL.

### Fixed

- A subquery in a scan condition read the rows of another subquery (#237). `key_column_usage`, `constraint_column_usage` and `role_usage_grants` now give the rows of PostgreSQL.

## 0.1.15 (2026-10-10)

A patch release on the way to M2. rupg now has all the regular expression functions of PostgreSQL and runs set-returning functions in the target list. Thus the setup makes all the views of `system_views.sql`.

### Added

- The regular expression functions that find each match (#227): `regexp_replace`, `regexp_count`, `regexp_instr`, `regexp_substr`, `regexp_matches`, `regexp_split_to_table` and `regexp_split_to_array`.
- Set-returning functions in the target list (#228). The calls run side by side and give nulls for the shorter sets, as the `ProjectSet` node of PostgreSQL does, and a call in a wrong place gives the error of PostgreSQL. The view `pg_stats_ext_exprs` is now in the catalog, with `row_security_active` and the functions of extended statistics.

## 0.1.14 (2026-10-10)

A patch release on the way to M2. rupg now has rows as values and the regular expression functions that give the text of a match. Thus 9 of the 11 functions in SQL of `information_schema` now run.

### Added

- Whole-row references and the fields of a row value (#223). `SELECT t FROM ... AS t`, `t.*` as an argument and `(x).field` give the results of PostgreSQL, and the setup makes `_pg_truetypid` and `_pg_truetypmod` of `information_schema`.
- The positions of regular expression matches and groups (#224). The engine builds the tree of subexpressions of PostgreSQL, so each group gets the same text. `substring` with a POSIX or an SQL pattern, `similar_to_escape`, `SIMILAR TO`, `regexp_match` and `regexp_like` run.
- `format_type` of an interval with a typmod, such as `interval day to second(3)` (#225). `_pg_interval_type` of `information_schema` gives the results of PostgreSQL.

## 0.1.13 (2026-10-10)

A patch release on the way to M2. rupg now has domains and functions in SQL, and the setup of a new cluster makes the domains and the first functions of `information_schema`.

### Added

- Domains (#220). The setup makes the five domains of `information_schema`, such as `cardinal_number` and `sql_identifier`, with the OIDs of PostgreSQL. A cast to a domain checks its constraints, and `pg_type`, `pg_constraint` and `pg_depend` show the rows of PostgreSQL.
- Functions in SQL with a `RETURN` body (#221). The setup makes seven functions of `information_schema`, such as `_pg_char_max_length` and `_pg_numeric_precision`, and the engine runs the body of each function. Also the shift operators `<<` and `>>` of the integer types and `pg_encoding_max_length`.

## 0.1.12 (2026-10-09)

A patch release on the way to M2. rupg now runs the SQL statements of the prepared statements, and two more system views return the rows of PostgreSQL.

### Added

- `pg_prepared_statements` shows the statements of `Parse`, with the counts of the generic and the custom plans that `plan_cache_mode` chooses (#216).
- The SQL statements `PREPARE`, `EXECUTE` and `DEALLOCATE` (#217). They share the prepared statements with the extended protocol, and `EXECUTE` coerces its values and gives the errors of PostgreSQL.
- `pg_get_function_arguments`, `pg_get_function_identity_arguments`, `pg_get_function_result` and `pg_get_function_arg_default` give the text of PostgreSQL for each built-in function (#218). `pg_sequence_last_value`, `pg_my_temp_schema` and `pg_is_other_temp_schema` run, so `pg_sequences` and `pg_seclabels` return rows.

## 0.1.11 (2026-10-09)

A patch release on the way to M2. rupg now has the rules of the time zone data, and more system views return the rows of PostgreSQL.

### Added

- The functions of the statistics of the whole server, so `pg_stat_bgwriter`, `pg_stat_checkpointer`, `pg_stat_wal`, `pg_stat_io`, `pg_stat_archiver`, `pg_stat_slru`, `pg_stat_recovery_prefetch` and `pg_stat_lock` return their rows (#211). rupg does not keep these counters yet, so each counter is 0, as on a server that has not done the work.
- The views of the parts that rupg does not have, such as `pg_aios`, `pg_replication_origin_status`, `pg_publication_tables` and `pg_cursors`, return the rows of PostgreSQL on a server with none of them (#212).
- `pg_wait_events` gives the 287 wait events of PostgreSQL 19 (#213). The new task `cargo xtask waitevents` makes the table from the vendored `wait_event_names.txt`.
- The rules of the time zone data 2026e (#214). `TimeZone` takes every zone of the data and POSIX zone strings with rules, and `timestamptz` output, input and casts use the offsets of the zone, also across daylight saving time changes. The zone compiler is a port of `zic.c`, and its output is equal to the files of PostgreSQL for all 598 zones. `pg_timezone_names` and `pg_timezone_abbrevs` return rows.

## 0.1.10 (2026-10-09)

A patch release on the way to M2. The types `inet` and `cidr` are in, and the views of the sessions, of TLS, of replication and of progress return the rows of PostgreSQL.

### Added

- The types `inet` and `cidr`, with their input and output, casts, comparisons, operators and functions such as `host`, `masklen`, `network`, `broadcast` and `set_masklen` (#205).
- The rows `template0` and `postgres` of `pg_database`, next to `template1` (#206).
- A table of the sessions of the server, which `pg_stat_activity`, `pg_stat_get_activity` and the `pg_stat_get_backend_*` functions read (#207). Each session reports its state, the text of its statement, the start of its transaction and its `application_name`, and a transaction reads one snapshot of the table, as in PostgreSQL.
- `pg_stat_ssl` shows the TLS version, the cipher, the bits and the client certificate of each session, in the forms of OpenSSL (#208).
- The views of replication, of subscriptions, of prepared transactions and of the progress of commands return no rows, as on a server with none of them (#209). `pg_stat_wal_receiver` gives its row of nulls, `pg_indexam_progress_phasename` gives the names of the phases of btree and GIN, and `pg_stat_get_progress_info` rejects a bad command name.

## 0.1.9 (2026-10-09)

A patch release on the way to M2. The views `pg_settings`, `pg_views` and the statistics views of one object now return rows.

### Added

- `pg_show_all_settings` gives a row for each setting, so `pg_settings` shows the 438 settings with the columns of PostgreSQL (#201).
- The `pg_stat_get_*` functions of a relation, an index, a function and a database, with their `xact` forms (#202). rupg does not keep the counters yet, so each one gives the value of PostgreSQL for an object with no entry. The views `pg_stat_*_tables`, `pg_stat_*_indexes`, `pg_statio_*` and `pg_stat_database_conflicts` return a row for each object.
- `pg_get_viewdef` in its five forms gives the text of PostgreSQL for the views of the user and for 85 of the 86 system views (#203). `pg_views` shows the definition of each view.

## 0.1.8 (2026-10-09)

A patch release on the way to M2. A query can now read array subscripts and match regular expressions, and the system views of `pg_catalog` are in the catalog.

### Added

- A query can read the subscripts of an array, such as `a[2]`, `a[1:2]` and `m[1][2]`, also on the `int2vector` and `oidvector` columns of the catalog (#197).
- The regular expression operators `~`, `~*`, `!~` and `!~*` on `text`, `name` and `character`, with the errors of the compiler of PostgreSQL (#198). The `\d` queries of psql use them.
- rupg runs `system_views.sql` once in each process, as `initdb` does, so 85 of the 86 system views of `pg_catalog` are in the catalog with their OIDs, privileges and options (#199). `CREATE VIEW` accepts the options `security_barrier` and `security_invoker`.

## 0.1.7 (2026-10-09)

A patch release on the way to M2. A session can now create views, cast arrays element by element, and call the common array functions.

### Added

- `CREATE VIEW` and `CREATE OR REPLACE VIEW` make a view in the catalog, with its row type, its rule and the dependencies of its query (#193). A query reads a view as a subquery. The column names, types, typmods and collations, and the errors of a bad replacement, are those of PostgreSQL.
- A cast of an array casts each element, as `ArrayCoerceExpr` does (#194). This covers casts such as `text[]` to `name[]`, casts through the text form and length casts such as `varchar(2)[]`.
- The array functions `string_to_array`, `array_to_string`, `array_length`, `array_lower`, `array_upper`, `array_ndims`, `array_dims`, `cardinality`, `array_append`, `array_prepend` and `array_cat`, and the `||` operators of the arrays (#195).

## 0.1.6 (2026-10-09)

A patch release on the way to M2. A query can now run set operations and `VALUES` lists.

### Added

- A query can combine `SELECT` parts with `UNION`, `INTERSECT` and `EXCEPT`, with and without `ALL`, and with parentheses, `ORDER BY`, `LIMIT` and `OFFSET` on the result (#190). The columns get the common types of PostgreSQL, and a part can be a subquery in `FROM` or in an expression. A locking clause and a column type with no sort or hash operators give the errors of PostgreSQL.
- `VALUES` runs as a query and as a subquery in `FROM`, with `ORDER BY`, `LIMIT` and `OFFSET` (#191). The columns get the common types of the rows and the names `column1`, `column2` and so on. Rows of different lengths, `DEFAULT` and a locking clause give the errors of PostgreSQL.

## 0.1.5 (2026-10-09)

A patch release on the way to M2. A query can now read `LATERAL` subqueries and functions in `FROM`.

### Added

- A `LATERAL` subquery in `FROM` reads the columns of the parts of `FROM` before it (#187). The scan runs it again for each row of those parts, at the top of `FROM` and in inner and left joins. A reference to the left side of a full or a right join gives `42P10` with the detail of PostgreSQL.
- A query can call a function in `FROM`, with `ROWS FROM`, `WITH ORDINALITY`, column aliases and column definition lists (#188). The function can read the parts of `FROM` before it. The set-returning functions are `generate_series` of the integer types, `unnest`, `generate_subscripts`, `string_to_table`, `pg_options_to_table`, `aclexplode` and `pg_get_keywords`. A function of a scalar type gives one row.

## 0.1.4 (2026-10-09)

A patch release on the way to M2. A session can now read the tables of the user and subqueries in `FROM`, and the planner folds constant expressions as PostgreSQL does.

### Added

- `SELECT` reads the tables and the sequences that a user makes (#184). A table has no rows yet, because rupg has no storage for user data. A sequence gives its one row. The columns have the types, the typmods and the not-null flags of the catalog.
- A query can have a subquery in `FROM`, with column aliases and the name `unnamed_subquery` when it has no alias (#185). The planner pulls up a simple subquery and drops the columns that the query does not read, as PostgreSQL does. The errors of a name that only `LATERAL` could make visible have the hint of PostgreSQL. `LATERAL` itself gives `0A000`.

### Changed

- The planner folds the constant parts of each expression, as `eval_const_expressions` does (#183). An error of the fold, such as `SELECT 1/0`, comes before `RowDescription`, and an error of a function that runs with the rows comes after it. A part that PostgreSQL drops at plan time does not run. The extended protocol folds at `Bind` with the values of the parameters.
- Before the scan, the executor runs the parts of `WHERE` that read no column and call no volatile function, so a false condition stops the query before it reads rows (#185).

## 0.1.3 (2026-10-08)

A patch release on the way to M2. The catalog functions now know the objects that a user makes.

### Added

- `regclass`, `regnamespace` and `regtype` read and show the user schemas, relations and types, with the schema when the name alone does not find the object (#180). `to_regclass`, `to_regnamespace`, `to_regtype`, `pg_table_is_visible` and `pg_type_is_visible` use the same lookups.
- `format_type` is a port of `format_type_extended`. A user array type shows as `elem[]`, and a type without a `typmodout` function shows its typmod as a number (#180).
- `pg_get_expr`, `pg_get_constraintdef` and `pg_get_indexdef` show the defaults, checks, constraints and indexes of the user objects with the same text as PostgreSQL 19, in the normal and the pretty forms (#181). An object of a system catalog gives `0A000`, and a function with a SQL syntax, for example `EXTRACT`, shows as a plain call.

### Fixed

- Input of `pg_node_tree` gives `0A000` "cannot accept a value of type pg_node_tree", as in PostgreSQL (#181).

## 0.1.2 (2026-10-08)

A patch release on the way to M2. A session can now define tables and see them in the catalogs.

### Added

- A session runs `CREATE SCHEMA`, `CREATE TABLE` and `CREATE INDEX` (#177). All the sessions of a server share one catalog. A transaction that defines an object holds a lock on the catalog until it ends, and its changes become visible to the other sessions at `COMMIT`. At a rollback the changes go, but the OIDs stay used, as in PostgreSQL. The notices go to the client as `NoticeResponse`.
- The catalog tables `pg_namespace`, `pg_class`, `pg_type`, `pg_attribute`, `pg_attrdef`, `pg_constraint`, `pg_index`, `pg_sequence` and `pg_depend` show the objects that a user makes (#178). `CREATE TABLE` makes the TOAST table and its index with the OIDs of PostgreSQL. The columns of type `pg_node_tree` hold the analyzer form of the expression, not the PostgreSQL node text.
- `xid` and `cid` have text input, and `xideqint4` and `xidneqint4` work (#178).

## 0.1.1 (2026-10-08)

A patch release on the way to M2. The SQL that a session can run is the same as in 0.1.0.

### Added

- The analyzer can run `CREATE TABLE`, `CREATE INDEX` and `CREATE SCHEMA` on a catalog (#174). It makes the same rows, OIDs, names, notices and errors as PostgreSQL 19 for 102 cases from the oracle. A session does not send these statements to it yet, so they still give `0A000`.

### Fixed

- The header of `cargo xtask bench` gives the toolchain that made the instruction counts (#173).
- The `type_recv` fuzz target does not fail on an `interval` whose time is the smallest `int64`. PostgreSQL also refuses the output of that value as input (#175).

## 0.1.0 (2026-10-08)

The release that closes M1, the file. The single `.rupg` file, the buffer manager, the per-worker log, recovery, the hot store and the first MVCC are complete, and the three M1 fuzz targets ran 24 CPU hours each with no find. The close report is in issue #2. This release also has the first instruction count set and the first privilege functions. No crate is published.

### Added

- `set_config` (spec/06 section 6.13), which is `SET` as a function. It takes the same values and gives the same errors as `SET`, and gives the new value as `SHOW` does. A null value resets the parameter. The change counts for the rest of the query, so a `set_config` of `TimeZone` changes the output of the values after it in the same row. An error or a rollback undoes the change, and a local change ends with its transaction.

- The access privilege inquiry functions (spec/07 section 7.18.3): the `has_*_privilege` functions for tables, columns, sequences, schemas, functions, types, languages, tablespaces, databases, foreign data wrappers, foreign servers, large objects and parameters, and `pg_has_role`. All 90 forms of `pg_proc.dat` have a kernel. The checks are a port of `acl.c` and `aclchk.c`, with the walks over `pg_auth_members` and the roles `pg_read_all_data`, `pg_write_all_data`, `pg_maintain` and `pg_database_owner`. The errors come in the same order as in PostgreSQL. The server has one role, the bootstrap superuser, so a check for the current user always gives true.

- `cargo xtask bench` (spec/21 section 21.14). It runs the instruction count set of six cases under `perf stat` and fails when one case is more than 3 percent higher or the total is more than 1 percent higher than a saved count. `xtask/icount.txt` has the first counts.

- `cargo xtask lifts` (spec/22 section 22.7). It reads the history of each file that rupg lifts from rudb and lists each rudb change after the lift that nobody reviewed.

### Fixed

- After the first query of a transaction, `SET TRANSACTION ISOLATION LEVEL` with a new level, `SET TRANSACTION [NOT] DEFERRABLE` and `SET TRANSACTION READ WRITE` in a read-only transaction now give `25001`, as in PostgreSQL. A query takes the snapshot of the transaction when it is analyzed, in a `Query` or a `Parse` message.
- The static catalog rows have the privileges that `initdb` gives them. `relacl`, `attacl`, `nspacl` and `datacl` are no longer null where PostgreSQL has a value, and all 284 values of the rows below OID 10000 are the same as in PostgreSQL 19.

## 0.0.18 (2026-10-08)

The eighteenth patch release on the 0.0 line. `SELECT` now has subqueries in expressions, the 12 OID alias types such as `regclass` and `regtype`, and the catalog visibility and comment functions. No crate is published.

### Added

- The catalog information functions of spec/07 section 7.18.3: `pg_table_is_visible`, `pg_type_is_visible`, `pg_function_is_visible`, `pg_operator_is_visible`, `pg_opclass_is_visible`, `pg_opfamily_is_visible`, `pg_collation_is_visible`, `pg_conversion_is_visible`, the four `pg_ts_*_is_visible` functions and `pg_statistics_obj_is_visible`. Each one finds the object as `namespace.c` does, with the schemas of `search_path`, and gives a null for an OID that no object has. `pg_get_userbyid` gives the name of the role, or `unknown (OID=n)`. `obj_description`, `col_description` and `shobj_description` give the comments of `pg_description` and `pg_shdescription`, as the SQL functions of `system_functions.sql` do. The catalog has the built-in objects and comments only, so the objects and comments that the scripts of initdb add are not there yet.

- The input and output of the 12 OID alias types (spec/04 section 4.9): `regclass`, `regtype`, `regproc`, `regprocedure`, `regoper`, `regoperator`, `regnamespace`, `regrole`, `regcollation`, `regconfig`, `regdictionary` and `regdatabase`. The input takes a number, `-` for 0, or a name that it finds as `regproc.c` and `namespace.c` do, with the schemas of `search_path`. The output gives the name with its schema only when the name alone does not find the object, so `2836::regclass` is `pg_toast.pg_toast_1255`. The functions `to_regclass`, `to_regtype`, `to_regtypemod` and the others give a null for a name that does not exist, and the same error as PostgreSQL for a syntax error in a type name or a name with too many dots. The cast from `text` to `regclass` gives the error of `RangeVarGetRelid`. The binary format of each type is the format of `oid`. The catalog has the built-in objects only, so a name finds a built-in object or nothing.

- Subqueries in expressions (spec/23 section 23.5): `EXISTS`, `IN`, `NOT IN`, `op ANY`, `op ALL`, `ARRAY(SELECT ...)` and a scalar subquery, in the target list, `WHERE`, `JOIN ... ON`, `HAVING`, `GROUP BY`, `ORDER BY`, `LIMIT` and `OFFSET`. A subquery can read the columns of the queries outside it at any depth. The analyzer finds a name as `parse_relation.c` does, from the subquery up to the outer queries, so a bad name gives the same error and hint as PostgreSQL, also for a close name of an outer relation. It gives the errors of `transformSubLink` for a wrong number of columns, and `subquery uses ungrouped column` for a column of a grouped outer query. The executor runs a subquery that reads no outer column once, as an `InitPlan` does, and runs the other subqueries again for each row. It reads the tables of a subquery once. `EXISTS` skips the target list when `simplify_EXISTS_query` can drop it, so `EXISTS (SELECT 1/0 FROM t)` gives no error, and a scalar subquery with more than one row gives `21000`. A row on the left of `IN` or `ANY`, `ARRAY()` of an array, an aggregate of an outer query, and a subquery in `FROM` give `0A000`.

## 0.0.17 (2026-10-08)

The seventeenth patch release on the 0.0 line. `SELECT` now has aggregate functions, `GROUP BY` and `HAVING`, and two new fuzz targets check the input, output, receive and send functions of 52 types against PostgreSQL. No crate is published.

### Added

- The fuzz targets `type_input` and `type_recv` of M2 (spec/21 section 21.7). They call the input, output, receive and send functions of 52 types with the settings that change the text, and check that each value reads back to the same output in the text and the binary formats. The example `type_replay` compares a corpus with the oracle. The nightly workflow runs both targets.

- Aggregate functions, `GROUP BY` and `HAVING` (spec/23 section 23.5). The analyzer makes an aggregate call as `parse_agg.c` does, with `DISTINCT`, `ORDER BY` and `FILTER` in the call, and gives the same errors as PostgreSQL for an aggregate call in a wrong place, a nested call, and a column that is not in `GROUP BY`. A column of a table whose primary key is in `GROUP BY` is grouped too. The executor runs the transition and final functions of `pg_aggregate` for `count`, `sum`, `avg`, `min`, `max`, `bool_and`, `bool_or`, `every`, `bit_and`, `bit_or`, `bit_xor`, `any_value`, `string_agg`, `array_agg` and the `float4` and `float8` forms of `var_pop`, `var_samp`, `variance`, `stddev_pop`, `stddev_samp` and `stddev`. The sums and the averages of `bigint` and `numeric` keep their state as `numeric.c` does, with the count of `NaN` and of the infinite values. The groups come out in the order of their keys, and the rows of a group keep the order of the scan. A `GROUP BY` on a type that has `=` and no order, such as `xid`, keeps the groups in the order of their first rows. A `GROUP BY` that can neither sort nor use a hash table gives `could not implement GROUP BY`, as PostgreSQL does. The ordered-set aggregates, the window functions, `GROUPING SETS`, `ROLLUP`, `CUBE`, the JSON aggregates, `array_agg` of an array, and the `var_pop` and `stddev` forms of the integer and `numeric` types give `0A000`.

### Fixed

- The shortest text of a `float4` or `float8` value is the text of Ryu in PostgreSQL. A text that is exactly halfway to the next value is not the shortest text, so `1e23::float8` is `9.999999999999999e+22` and `42483247::float4` is `4.2483248e+07`. When a value is exactly halfway between two shortest texts, the text with the even last digit comes out, so `-3222613.25::float4` is `-3.2226132e+06`.
- The binary input of `jsonb` checks the encoding of the text before the JSON syntax, as `pq_getmsgtext` does. A zero byte gives `invalid byte sequence for encoding "UTF8": 0x00`, and a bad byte shows its value.
- The input of `json` and `jsonb` reads the token after a scalar before the action of the scalar, as `parse_scalar` does. So `'[1e999999 x]'::jsonb` gives the syntax error of the token `x` and not `value overflows numeric format`.
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
