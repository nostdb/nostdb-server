# Database daemon distribution candidates

These files are review candidates and perform no publication, service registration, or image push by themselves.

- `release-manifest.json`, `scripts/stage_npm_candidate.py`, and the `npm/` package tree define the unpublished `@nostosdb/server` launcher plus six exact native packages. `scripts/verify_local_npm.py` proves an offline isolated global install exposes both `nostosd` and `nostos` through the exact matching CLI package.
- `systemd/nostosdb.service` runs `/usr/local/bin/nostosd serve --config /etc/nostosdb/server.toml` as a dedicated account with `/var/lib/nostosdb` as its only writable database state. Its [initialization procedure](systemd/README.md) runs `nostosd init` as that account so generated `0600` credentials are readable by the service.
- `homebrew/Formula/nostosdb.rb.in` defines formula/service name `nostosdb`, installs the combined `nostos`/`nostosd` candidate, and exposes `nostosd` through `brew services`. Its caveat pre-creates per-user `data`, `config`, and `logs` directories with mode `0700` before explicit loopback-only initialization; Homebrew's temporary `post_install` HOME must never hold persistent state.
- `windows/install-service.ps1` fails closed with an explicit unsupported diagnostic. The current `nostosd.exe` is a foreground console process; it has no Windows Service Control Manager entry point or reviewed credential ACL installer and must not be registered directly with `sc.exe`.
- `Dockerfile` and `compose.yaml` build from the NostosDB root context and use `/etc/nostosdb/server.toml` plus the `/var/lib/nostosdb` authoritative volume.

All service forms execute the same versioned configuration, catalog, credentials, and data-directory runtime. Installation scripts must restrict the service identity and credential files for their platform before production use.
