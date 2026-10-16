# rupg-snowball

This crate has the 33 stemmers of Snowball that the module `dict_snowball` of PostgreSQL 19 has. The text search dictionaries of the template `snowball` use them, for example `english_stem`.

## Where the files come from

PostgreSQL 19 is synced with the Snowball commit `2d2e312df56f2ede014a4ffb3e91e6dea43c24be` of 2025-12-15 (see `src/backend/snowball/README` in PostgreSQL). The files of this crate come from the same commit of https://github.com/snowballstem/snowball.

- `src/algorithms/*_stemmer.rs` is the output of `make` in the Snowball tree, from `rust/src/snowball/algorithms/`. Snowball 3.0.0 generates each file from `algorithms/X.sbl` with `snowball X.sbl -rust`. There are two changes. In the two `use` lines, `snowball::SnowballEnv` and `snowball::Among` are now `crate::env::SnowballEnv` and `crate::among::Among`. Then `rustfmt` formats the files with the settings of the workspace.
- `src/among.rs` is `rust/src/snowball/among.rs`, and `src/env.rs` is `rust/src/snowball/snowball_env.rs`, with the same two changes.
- `src/algorithms/mod.rs` and `src/lib.rs` are new. `lib.rs` has the table of the stemmers in the order of `stemmer_modules` in `dict_snowball.c`.

To update the crate, check out the new commit of the PostgreSQL pin in a Snowball tree, run `make`, copy the files again and make the same two changes. Then make `tests/oracle.tsv` again from the new PostgreSQL.

## The test

`tests/oracle.tsv` has 120 words for each language. The words come from the file `voc.txt` of https://github.com/snowballstem/snowball-data, and the stems come from `ts_lexize` on PostgreSQL 19 with a dictionary of the template `snowball` and no stop words.

## License

The code is under the BSD 3-clause license of Snowball. See `LICENSE`.
