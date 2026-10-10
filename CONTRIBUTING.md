# Contributing

Thank you for your help. This file tells you how to make a change that we can merge.

## Before you start

Read the part of [`spec/`](spec/) that your change touches. The design is written before it is built. If your change does not agree with the spec, change the spec first, in the same pull request or in an earlier one. A decision in [`spec/00-README.md`](spec/00-README.md) under "Settled decisions" changes only after you write the measurement that shows it is wrong in [`spec/24-open-questions.md`](spec/24-open-questions.md).

For a large change, open an issue first. Use the milestone issues (label `kind/milestone`) to find the current work.

## Running the checks

CI runs these checks. Run them before you push.

```sh
cargo fmt --all --check
cargo xtask layers
cargo xtask style
cargo xtask vendor --check
cargo xtask grammar
cargo clippy --workspace --all-targets --all-features
cargo test --workspace --all-features
```

`cargo xtask layers` checks the layer rule. `cargo xtask style` checks the `unsafe` rules and the writing rules. `cargo xtask vendor --check` checks the vendored files against their hashes. `cargo xtask grammar` builds the LALR(1) tables of `gram.y` and `pl_gram.y` and fails if a grammar has more conflicts than its `%expect` allows.

To compare the tables with bison, make a report with `bison -Dlr.default-reduction=accepting -v file.y` and run `cargo xtask grammar --compare file.y file.output`. The command matches the states by their kernel items and compares each shift, reduction, error action and goto.

`cargo xtask lifts [dir]` checks the files that rupg lifts from rudb (spec/22 section 22.7). It reads the history of each source file in a rudb checkout, `../rudb` when you give no directory, and lists each rudb commit after the lift that nobody reviewed. The checkout must have the full history. When you port a change or decline it, add a line `// Reviewed at <commit> (<date>): <decision>.` under the provenance header of the file. CI has no rudb checkout, so run this task before each milestone release.

`cargo xtask bench` runs the instruction count set of spec/21 section 21.14. It builds `crates/rupg/examples/icount.rs` in release mode, runs each case under `perf stat -e instructions:u` and prints the user space instructions of each case. Each case runs three times and the task takes the median. `--save <file>` writes the counts, and `--check <file>` fails when one case is more than 3 percent higher than the saved count or the total is more than 1 percent higher. The task needs `perf` and a machine that shows the hardware counters, and most virtual machines do not. Counts change with the processor and the toolchain, so compare only counts from the same machine. `xtask/icount.txt` has the counts at each release, with the machine and the toolchain in its header. To add a case, add it to `CASES` in the example and add its count to the file in the same pull request.

`cargo xtask idle` runs the idle session test of spec/06 section 6.4.3 on Linux. It builds `rupg` and `crates/rupg-server/examples/idle.rs` in release mode, starts `rupg serve` for each variant, opens 1,000 idle sessions and divides the increase of the proportional set size of the server by the number of sessions. The variants are trust over a Unix socket, trust over TCP, trust over TLS, SCRAM over TLS after `DISCARD ALL`, and trust over a Unix socket after `PREPARE` and `DEALLOCATE`. The task fails when one session of a variant costs more than 64 KiB. `--debug` uses the dev profile, which needs less disk. `--sessions N` changes the number of sessions. `--postgres <data dir>` also measures a running PostgreSQL server over its Unix socket with `--postgres-sessions N` sessions, 90 by default, so that the two numbers come from the same machine.

## What a change must include

1. A test for each behavior that the change adds or fixes. A fix for a wrong answer includes the query that showed it.
2. A spec change if the change differs from the spec.
3. A line in [`CHANGELOG.md`](CHANGELOG.md) under "Unreleased" if a user can see the change.
4. For a performance change, the numbers before and after, the machine and the command. Use `rupg-bench`. A timing from a shared CI runner is not a number.

## Compatibility work

PostgreSQL 19 is the reference. The pin is `REL_19_STABLE` at `7d3d2db7`. When rupg and PostgreSQL disagree, PostgreSQL is right unless [`spec/05-compatibility.md`](spec/05-compatibility.md) lists the difference. A difference that the spec does not list is a bug with the label `kind/compat`.

The oracles and the suites are in [rupg-compat](https://github.com/tamnd/rupg-compat). Run the differential there to compare rupg with a real PostgreSQL server on the same input.

## Clean room

rupg is a clean room implementation under Apache-2.0. Do not read or copy code under the AGPL, the GPL or another copyleft license into this repository. You may read the PostgreSQL source, which is under the PostgreSQL License. A file that is vendored from PostgreSQL goes in `vendor/` with a `PIN` file that gives the commit and the SHA-256.

## The layer rule

Each crate has a rank in `xtask/layers.toml`. A crate may depend only on a crate of a lower rank. A new crate needs a new line in that file, and the reviewer of that line reviews the design. See [`spec/22-crate-layout.md`](spec/22-crate-layout.md) section 22.3.

## Unsafe code

Only six crates may contain `unsafe`: `rupg-platform`, `rupg-buffer`, `rupg-kernels`, `rupg-jit`, `rupg-capi` and `rupg-wasm`. Each other crate has `#![forbid(unsafe_code)]`. Each `unsafe` block must have a `// SAFETY:` comment directly above it that states the invariant.

## Writing style

The spec, the README files, the issues and the comments use ASD-STE100 technical English.

- Write short sentences in the active voice.
- Use one line for each paragraph. Do not break a sentence over two lines.
- Do not use the em dash or the en dash.
- Do not use a horizontal rule as a page break.
- Give the source of each number.

`cargo xtask style` checks the dashes, the rules and the line breaks in Markdown.

## Commits and pull requests

- Use one topic for each pull request.
- Write the commit subject in the imperative mood, for example "Add the header slot reader".
- We merge with squash. The pull request title becomes the commit subject.
- The `ci` check must pass before a merge.

## Making a release

The version is `0.M.patch` (spec/23 section 23.17). The release at the close of milestone M is `0.M.0`. The releases between two closes are patch releases.

1. In a pull request, run `cargo xtask version <x.y.z>`. It sets the version in `[workspace.package]`, each exact internal pin in `[workspace.dependencies]`, and `Cargo.lock`.
2. In the same pull request, rename the "Unreleased" section of `CHANGELOG.md` to `## <x.y.z> (<date>)` and add a new empty "Unreleased" section above it.
3. After the merge, tag the merge commit with `v<x.y.z>` and push the tag. The release workflow checks that the tag matches the workspace version and that the changelog has the section. Then it builds the binaries and publishes the GitHub release.

## Reporting a wrong answer

Use the "Wrong answer" issue template. Give the schema, the data, the query, the rupg result and the PostgreSQL 19 result. A wrong answer has priority over all other work.

## Security

Do not open a public issue for a security problem. Read [SECURITY.md](SECURITY.md).

## License

By contributing, you agree that your contribution is licensed under Apache-2.0.
