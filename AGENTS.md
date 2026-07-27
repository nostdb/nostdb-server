# nostdb-server Agent Instructions

## Inheritance

This repository is a child of the NostDB root superproject. The root `AGENTS.md`
at <https://github.com/nostdb/nostdb> is the governing contract.

This file only narrows the root rules for the local daemon boundary. It must not weaken
any root product, safety, or ownership boundary. If this file and the root contract
appear to conflict, the root contract wins, the current valid behavior stays unchanged,
and the exact conflict is recorded in the root `IMPLEMENTATION_PROGRESS.md`.

## Language policy

Write everything in this repository in English only.

This covers documentation, source code, identifiers, comments, rustdoc, test names,
commit messages, branch names, pull request titles and bodies, issue text, diagnostics,
error messages, log records, configuration, fixtures, and every line the daemon emits.

This rule holds regardless of the language a request is written in.

## Ownership boundary

`nostdb-server` coordinates local access to databases the Engine owns. It implements no
database behavior.

Permitted:

- the daemon lifecycle, and the OS lock enforcing one instance per OS user;
- the local IPC endpoint and its framing;
- the named database catalog and its serialized mutation;
- sessions, query timeouts, and per-session resource limits;
- crash recovery and stale-session cleanup;
- the local authentication boundary.

Prohibited:

- a parser, storage engine, synchronizer, analyzer, or query engine;
- any `.nostdb` writer, because only `nostdb-core` writes `.nostdb`;
- a command surface, REPL, or output formatter, which belong to `nostdb-cli`;
- a TCP or HTTP listener, in the MVP;
- a bundled GitHub provider implementation;
- a second copy of the `.nost` grammar or the conformance fixtures;
- a copy of the root PRD;
- code copied in from any legacy implementation.

If a responsibility appears to need one of the prohibited items, it needs a public
`nostdb-core` API instead. Add the API there and call it.

## Invariants this repository must never break

- At most one daemon runs per OS user, enforced by an OS lock rather than by a
  liveness guess.
- The endpoint is reachable only by the current OS user. A client from another OS user
  is denied, and the denial is not configurable away in the MVP.
- The MVP opens no TCP or HTTP listener.
- Only the Engine writes `.nostdb`. Every mutation goes through a public Core API.
- A path-based command never requires this daemon. Embedded Mode is the default, and the
  daemon is for named databases.
- The daemon does not make an unrelated named database visible to a query. A query sees
  its target database and the links that database declares.
- Catalog mutation is serialized, and a failed mutation leaves the previous catalog
  intact.
- A failed operation preserves the last valid database generation.
- The local protocol carries its own `server_protocol_version`, negotiated explicitly. An
  unsupported version is refused by name rather than assumed compatible.
- Secrets never reach a log record, a diagnostic, the catalog, or any daemon output.
- A crash is recovered from persisted state, not from an in-memory assumption that did
  not survive it.

## Rust standards

Rust stable and Edition 2024. Public APIs require explicit error types and rustdoc. Use
`#![forbid(unsafe_code)]` where practical; required `unsafe` code needs a separate ADR
with documented safety invariants and a Miri or equivalent verification plan before
implementation.

This repository owns a long-running process, so it uses `tracing` for its log records and
does not write diagnostics directly to stdout.

Every change must pass:

```bash
cargo fmt --check
cargo check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

Do not add a dependency without documenting its purpose, maintenance status, and
license.

## Repository verification

Run before every commit:

```bash
./scripts/verify-repository.sh
```

The verifier is non-mutating. Extend it as the daemon lands rather than replacing it with
a manual checklist.

## Testing expectations

Treat every client request, every catalog entry, and every path as untrusted input.

Each boundary carries its own coverage:

- lifecycle: start, status, stop, foreground run, a second start while healthy, and a
  stale lock left by a killed process;
- endpoint: current-user access, denial across a user boundary, and a refused
  unsupported protocol version;
- catalog: add, remove, list, a duplicate name, a missing target, recovery from a
  truncated file, and concurrent mutation;
- sessions: concurrency, transaction isolation, timeouts, resource limits, and
  stale-session cleanup;
- recovery: an interrupted transaction, and a preserved last valid generation.

An operation that changes state without a test proving what it preserves on failure is
incomplete.

## Safety and external actions

- Never execute analyzed source code.
- Do not create remote repositories, add remotes, push to a new remote, publish
  packages, create releases, or modify registries without explicit user authorization.
- Never place credentials, passwords, tokens, private keys, or PEM content in files,
  fixtures, diagnostics, the catalog, or daemon output.
- Do not use destructive Git commands or broad deletion.
- Do not widen the endpoint's permissions to work around a local access failure.
- Preserve existing user changes and never revert them without authorization.

## Stage workflow

Implementation sequencing is tracked in the root `IMPLEMENTATION_PROGRESS.md`, not in
this repository. Do not begin a later Stage during a setup-only request, and do not mark
a Stage `DONE` until every Acceptance Criterion passes.
