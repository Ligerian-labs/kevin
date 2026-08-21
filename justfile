# Kevin developer commands. `just ci` is the gate every PR must pass.

set positional-arguments := true

# Postgres (pgvector) used by integration tests; `just db-up` starts one on this URL.
export DATABASE_URL := env_var_or_default("DATABASE_URL", "postgres://kevin:kevin@localhost:5433/kevin")

compose_file := "deploy/compose/postgres.yml"

# Default: run the full CI gate.
default: ci

# Format the whole workspace.
fmt:
    cargo fmt --all

# Check formatting without writing.
fmt-check:
    cargo fmt --all -- --check

# Clippy with warnings denied (all targets, all features).
clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# cargo-deny: advisories, licenses, bans, sources.
deny:
    cargo deny check

# Run the test suite with nextest (pass extra args through, e.g. `just test -p kevin-domain`).
test *ARGS:
    cargo nextest run --workspace --all-features "$@"
    cargo test --workspace --all-features --doc

# Full gate: fmt --check, clippy -D warnings, cargo-deny, nextest.
ci: fmt-check clippy deny test

# Start the pgvector Postgres from deploy/compose/postgres.yml (podman compose, else docker compose).
db-up: (_compose "up" "-d" "--wait")

# Stop the pgvector Postgres (volume is kept; add `-v` manually to wipe it).
db-down: (_compose "down")

# Validate the compose file with the available compose provider.
compose-check: (_compose "config" "--quiet")

# Open psql against DATABASE_URL.
psql:
    psql "$DATABASE_URL"

[private]
_compose *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    if command -v podman >/dev/null 2>&1 && podman compose version >/dev/null 2>&1; then
        exec podman compose -f "{{compose_file}}" "$@"
    elif command -v docker >/dev/null 2>&1; then
        exec docker compose -f "{{compose_file}}" "$@"
    else
        echo "error: neither 'podman compose' nor 'docker compose' is available" >&2
        exit 1
    fi
