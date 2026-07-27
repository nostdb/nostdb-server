#!/usr/bin/env bash

# Non-mutating verification for nostdb-server.
#
# Covers the repository shape, the ownership boundaries in AGENTS.md, the local-only transport
# invariant, the library's stdout boundary, and the Rust command set.

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository_root=$(CDPATH= cd -- "$script_dir/.." && pwd)

cd "$repository_root"

required_files="
AGENTS.md
CLAUDE.md
README.md
LICENSE
.gitignore
.editorconfig
.github/workflows/verify.yml
Cargo.toml
Cargo.lock
rust-toolchain.toml
src/lib.rs
src/main.rs
"

for required_file in $required_files; do
  if [ ! -e "$required_file" ]; then
    echo "missing required file: $required_file" >&2
    exit 1
  fi
done

# LICENSE is verbatim upstream text and is intentionally not whitespace-scanned.
checked_text_files="
AGENTS.md
README.md
.gitignore
.editorconfig
.github/workflows/verify.yml
Cargo.toml
rust-toolchain.toml
scripts/verify-repository.sh
"

for checked_file in $checked_text_files; do
  if grep -nE '[[:blank:]]+$' "$checked_file"; then
    echo "trailing whitespace found in: $checked_file" >&2
    exit 1
  fi
done

if [ ! -L CLAUDE.md ] || [ "$(readlink CLAUDE.md)" != "AGENTS.md" ]; then
  echo "CLAUDE.md must be a symlink to AGENTS.md" >&2
  exit 1
fi

if ! grep -q '^ *Server Side Public License$' LICENSE; then
  echo "LICENSE must be the Server Side Public License, Version 1" >&2
  exit 1
fi

if ! grep -q '^ *VERSION 1, OCTOBER 16, 2018$' LICENSE; then
  echo "LICENSE must be the Server Side Public License, Version 1" >&2
  exit 1
fi

# Section 13 is the clause that distinguishes the SSPL from the GPL family.
# Requiring it also detects a truncated license file.
if ! grep -q 'Offering the Program as a Service' LICENSE; then
  echo "LICENSE is missing Server Side Public License section 13" >&2
  exit 1
fi

# docs/PRD.md sections 21.1 and 30.6 make "no TCP or HTTP listener" a product invariant of
# the MVP daemon rather than a current limitation, and this is the only check that holds it.
# It runs now rather than with the crate, so the first transport added cannot quietly bring
# a remote listener with it. A local-only daemon needs none of these types.
if [ -d src ] && grep -rnE '\b(TcpListener|TcpStream|HttpServer|UdpSocket)\b' src; then
  echo "the MVP daemon is local-only and must not open a TCP, UDP, or HTTP listener" >&2
  exit 1
fi

# The same invariant, one level up: an HTTP or web-server dependency would provide the
# listener even if this repository never names the type itself.
if [ -f Cargo.toml ] && grep -nE '^(axum|actix-web|warp|hyper|rocket|tide|tonic) *=' Cargo.toml; then
  echo "the MVP daemon is local-only and must not depend on an HTTP or RPC server crate" >&2
  exit 1
fi

# AGENTS.md requires the library to use log records rather than writing diagnostics to stdout.
# The binary legitimately prints: it is a command, and reporting the endpoint it bound is its
# output. The library is what must stay quiet, because a caller parsing the binary's stdout must
# not have library chatter interleaved into it.
#
# main.rs is excluded by name rather than by directory, so a second binary added later is
# excluded deliberately instead of by accident.
if [ -d src ]; then
  noisy=$(
    find src -name '*.rs' ! -name 'main.rs' -exec grep -nE '\b(println!|print!)' {} + || true
  )
  if [ -n "$noisy" ]; then
    echo "the library must not write to stdout; use a log record instead" >&2
    printf '%s\n' "$noisy" >&2
    exit 1
  fi
fi

# The daemon coordinates access to databases the Engine owns. A parser, storage engine, or
# query engine here would be a second implementation of behavior the product contract
# defines once, and only nostdb-core writes .nostdb.
if [ -e grammar ] || [ -e fixtures ]; then
  echo "the grammar and the conformance fixtures belong to nostdb-spec" >&2
  exit 1
fi

if [ -e docs/PRD.md ]; then
  echo "the PRD lives once, in the root superproject" >&2
  exit 1
fi

# The Engine dependency is pinned to an exact commit. docs/REPOSITORIES.md requires a
# reproducible build and forbids following a floating branch, and a `branch =` or `tag =`
# dependency would do exactly that. A path dependency would break this repository's
# promise to build without its siblings, which is what CI checks out.
#
# The trigger is a dependency *declaration*, not any mention of the name. Matching any mention
# fired on this manifest's own dependency review, which explains in prose why the Engine is not
# depended on yet, and a check that a comment can fail is a check people learn to work around.
if [ -f Cargo.toml ] && grep -qE '^[[:space:]]*nostdb-core[[:space:]]*=' Cargo.toml; then
  if ! grep -qE '^nostdb-core = \{ git = "https://github.com/nostdb/nostdb-core\.git", rev = "[0-9a-f]{40}" \}$' Cargo.toml; then
    echo "nostdb-core must be pinned to an exact 40-character commit over https" >&2
    exit 1
  fi
fi

git diff --check

if [ -f Cargo.toml ]; then
  if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is required to verify the daemon" >&2
    exit 1
  fi
  cargo fmt --check
  cargo check --all-targets --all-features
  cargo clippy --all-targets --all-features -- -D warnings
  cargo test --all-targets --all-features
fi

echo "nostdb-server verification passed"
