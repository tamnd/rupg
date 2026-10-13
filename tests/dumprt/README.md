# The pg_dump round trip

This test does the `pg_dump` round trip of spec/21 section 21.4.1. It runs each night in the `dumprt` job of `.github/workflows/nightly.yml`.

The test runs `schema.sql` on rupg and on PostgreSQL 19. Then it does these steps:

1. It dumps the rupg database as text and in the custom format.
2. It restores the custom format dump into a new PostgreSQL database with `pg_restore`.
3. It dumps the restored database and the PostgreSQL database that ran `schema.sql`.
4. It compares the three text dumps. The test fails when one line is different.

The comparison skips the `\restrict` and `\unrestrict` lines, because they have a random key, and the `-- Dumped from` line, because it has the version of the server.

The test uses `pg_dump` of PostgreSQL 19 only. A `pg_dump` of version 14 to 18 does not dump a server of a newer major version, so the dumps of the older versions come with the shims of versions 14 to 18.

## Run the test

The test needs the programs of PostgreSQL 19 at the pin of `CONTRIBUTING.md`: `initdb`, `pg_ctl`, `createdb`, `dropdb`, `psql`, `pg_dump` and `pg_restore`.

```sh
cargo build -p rupg-cli
python3 tests/dumprt/roundtrip.py --pg-bin /path/to/postgres/bin --rupg target/debug/rupg
```

Without `--oracle`, the test makes a new PostgreSQL cluster with `initdb` in the work directory and stops it at the end. With `--oracle HOST:PORT`, the test uses a running server. It makes two databases with the port of rupg in their names and drops them at the end. Use `--work DIR` to keep the dumps and the logs.

## Add to the schema

Add a statement to `schema.sql` when rupg can run it. One statement goes on each line. User tables have no rows, because rupg cannot insert rows yet.
