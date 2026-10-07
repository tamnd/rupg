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
- CI on Linux, macOS and Windows, nightly sanitizers and Miri, a release workflow with build provenance, and security scans.
