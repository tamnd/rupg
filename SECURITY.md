# Security policy

## Supported versions

rupg is before version 1.0. We fix problems on the `main` branch only.

## Reporting a vulnerability

Use the private vulnerability reporting of GitHub on this repository: Security, then Report a vulnerability. Do not open a public issue for a problem that an attacker can use.

We reply in 7 days. If you get no reply in 14 days, open a public issue that asks for a reply and does not describe the problem.

## What is in scope

A `.rupg` file, a query, a protocol message and a `pg_dump` archive are all input that we do not trust. These are in scope:

- Memory unsafety on any input. The `unsafe` code is in six crates: `rupg-platform`, `rupg-buffer`, `rupg-kernels`, `rupg-jit`, `rupg-capi` and `rupg-wasm`.
- A crash, a hang or memory growth without a limit on a bounded input.
- A client that reads or changes data without the privilege to do so. This includes row level security, column privileges, `SECURITY DEFINER` functions and `search_path` attacks.
- An authentication bypass in SCRAM, `pg_hba.conf` or TLS.
- A sequence of C API calls that follows the header and causes unsoundness.

A clean error on a corrupt file is the correct behavior. It is not a vulnerability.

A wrong answer is usually not a security problem. Report it with the "Wrong answer" issue template. If an attacker can cause it, report it here.

## What we do

We confirm the problem, fix it on a private branch, and publish a release with an advisory. The advisory gives the problem, the affected versions and the fix. We credit the reporter unless the reporter asks us not to.
