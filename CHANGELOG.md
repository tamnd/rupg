# Changelog

All changes that a user can see are in this file. The version is 0.M.patch, where M is the last milestone that closed. See [`spec/23-milestones.md`](spec/23-milestones.md) section 23.17.

## Unreleased

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
- CI with the matrix of `spec/21-testing.md` section 21.16, nightly sanitizers and Miri, a release workflow with build provenance, and security scans.
