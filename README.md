# nostdb-server

`nostdb-server` is the NostDB per-user local daemon. It owns the one-instance-per-OS-user
process, the named database catalog, local IPC, sessions, recovery, and local resource
limits.

NostDB is a clean-slate, local-first Property Graph Database for software
environments.

## Boundary

This repository is a local coordination layer. It calls the public `nostdb-core` API and
implements no database behavior of its own.

It owns:

- the daemon lifecycle, and the OS lock that enforces at most one instance per OS user;
- the local IPC endpoint: a Unix domain socket, or a current-user ACL-protected named
  pipe on Windows;
- the named database catalog persisted under `~/.nostdb/catalog.json`;
- sessions, transaction isolation as the Engine exposes it, query timeouts, and
  per-session resource limits;
- crash recovery, stale-session cleanup, and serialized catalog mutation;
- the local authentication boundary, which trusts the OS-authenticated current user and
  rejects clients from any other user.

It does not own:

- a parser, storage engine, synchronizer, analyzer, or query engine, which belong to
  `nostdb-core`;
- any `.nostdb` writer, because only `nostdb-core` writes `.nostdb`;
- the command surface, the REPL, or the output formats, which belong to `nostdb-cli`;
- the GitHub provider, which is a separate out-of-process executable;
- the `.nost` grammar and conformance fixtures, which belong to `nostdb-spec`.

A responsibility that appears to need one of those calls the Engine instead. Duplicating
any of them here would create a second implementation of behavior the product contract
defines once.

## No remote listener

The MVP daemon is local-only. It does not open a TCP or HTTP listener, and it does not
accept a cross-host client. That is a product invariant rather than a current
limitation, and the repository verifier enforces it structurally.

Passwords are not implemented, because the endpoint is already restricted to the current
OS user by the operating system.

## Current status

Repository scaffolding only. The daemon, its protocol, and the catalog land in the
Stage 8 increments; see the implementation progress record in the root superproject.

## Product contract

The normative product contract is the PRD in the root NostDB superproject at
<https://github.com/nostdb/nostdb>. Executable format, grammar, and protocol contracts
live in <https://github.com/nostdb/nostdb-spec>, including the local protocol's own
`server_protocol_version`.

This repository keeps no copy of the PRD. A divergent child copy would create two
competing contracts.

## Verify

```bash
./scripts/verify-repository.sh
```

Continuous integration runs the same verifier on every push and pull request, so a
local pass and a CI pass check identical invariants.

## License

SSPL-1.0. See [LICENSE](LICENSE).

`nostdb-server` is **source-available**, not open source. `nostdb-core` and
`nostdb-cli` carry the same license. `nostdb-spec` and the Agent Skills are
Apache-2.0 so that any implementation can verify itself against the published
contracts.
