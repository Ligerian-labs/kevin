#!/bin/sh
# Entrypoint of the Kevin Kohral runtime image (`plan/08-kohral-runtime.md` §6).
#
#   1. source the provider-key secret env file, if Kohral mounted one;
#   2. resolve the database: the `kevin-database-url` secret file Kohral
#      mounts (`KEVIN__DATABASE__URL_FILE`), an explicit `KEVIN__DATABASE__URL`,
#      or — for a hand-run stack — `POSTGRES_*` plus the password file;
#   3. validate what the runtime cannot start without — the bearer token file
#      and a database — and fail *fast* with a message naming the secret;
#   4. `kevin db migrate`, retrying until the `memory` service accepts
#      connections (the stack starts both containers at once);
#   5. seed `MEMORY.md` **only when it is absent** — the file belongs to the
#      agent, a redeploy must never overwrite what it has recorded;
#   6. `exec kevin serve --kohral`, which sweeps non-terminal ledger rows to
#      `runtime_restarted` before it binds the port.
#
# NOT the only way the image starts. Kohral's `KevinRuntimeStrategy` gives the
# gateway service its own `/bin/sh -c "<seed MEMORY.md>; exec kevin serve
# --kohral"` command, which **replaces** this entrypoint: Kohral owns the
# MEMORY.md seed (its `KohralPlatformBriefingTest` asserts the strategy emits
# it) and mounts the database URL as a secret. Everything here is therefore
# written to be idempotent with, and skippable by, that command — which is also
# why the image sets `database.auto_migrate = true`: migrations must happen on
# the path where this script never runs.
#
# POSIX sh, no bashisms: it also runs under `sh -c` from a Kohral WorkloadSpec.
#
# Sourcing this file with `KEVIN_ENTRYPOINT_LIB=1` defines the functions
# without running anything, which is how `ac_ws23_*` tests the seeding logic.

set -eu

# Exit codes match `kevin`'s own table (crates/kevin-cli/src/ctx.rs):
#   3 = invalid arguments / missing configuration, 4 = dependency unreachable.
EXIT_CONFIG=3
EXIT_UNREACHABLE=4

CONFIG_DIR="${KEVIN_CONFIG_DIR:-/opt/kevin/config}"
DATA_DIR="${KEVIN_DATA_DIR:-/opt/kevin/data}"
MODEL_CACHE="${KEVIN_MODEL_CACHE:-/opt/kevin/models/embeddings}"
ENV_SECRET="${KEVIN_ENV_FILE:-/run/secrets/kevin-env}"
PG_PASSWORD_FILE="${KEVIN_POSTGRES_PASSWORD_FILE:-/run/secrets/postgres-password}"
DATABASE_URL_SECRET="${KEVIN_DATABASE_URL_FILE:-/run/secrets/kevin-database-url}"
# How long to wait for Postgres before giving up (seconds).
MIGRATE_TIMEOUT="${KEVIN_MIGRATE_TIMEOUT:-120}"

log() { printf 'kevin-entrypoint: %s\n' "$*" >&2; }
die() { code="$1"; shift; printf 'kevin-entrypoint: error: %s\n' "$*" >&2; exit "${code}"; }

# --- 1. provider keys ------------------------------------------------------
# `/run/secrets/kevin-env` is a `KEY=value` file with the credentials the agent
# CLIs need (ANTHROPIC_API_KEY, OPENAI_API_KEY, …). Sourced, never echoed; each
# worker's `env_passthrough` allow-list decides what actually reaches a CLI.
load_env_secret() {
    file="${1:-${ENV_SECRET}}"
    [ -r "${file}" ] || return 0
    set -a
    # shellcheck disable=SC1090
    . "${file}"
    set +a
    log "loaded credentials from ${file}"
}

# --- 2. database -----------------------------------------------------------
# Three shapes, in precedence order:
#   * `KEVIN__DATABASE__URL_FILE` / the `kevin-database-url` secret — what
#     Kohral's `KevinRuntimeStrategy` mounts, so the URL (with its password)
#     never appears in the process environment;
#   * `KEVIN__DATABASE__URL` — an operator pointing at their own Postgres;
#   * `POSTGRES_*` + the password file — the hand-run compose stack.
# `database.url` and `database.url_file` are mutually exclusive, so composing a
# URL is skipped as soon as a file is in play.
resolve_database() {
    if [ -n "${KEVIN__DATABASE__URL_FILE:-}" ]; then
        [ -s "${KEVIN__DATABASE__URL_FILE}" ] || die "${EXIT_CONFIG}" "KEVIN__DATABASE__URL_FILE=${KEVIN__DATABASE__URL_FILE} is missing or empty."
        export KEVIN__DATABASE__URL_FILE
        log "using the database URL from ${KEVIN__DATABASE__URL_FILE}"
        return 0
    fi
    if [ -s "${DATABASE_URL_SECRET}" ]; then
        KEVIN__DATABASE__URL_FILE="${DATABASE_URL_SECRET}"
        export KEVIN__DATABASE__URL_FILE
        log "using the database URL from ${DATABASE_URL_SECRET}"
        return 0
    fi
    compose_database_url
}

compose_database_url() {
    [ -z "${KEVIN__DATABASE__URL:-}" ] || return 0
    host="${POSTGRES_HOST:-memory}"
    port="${POSTGRES_PORT:-5432}"
    user="${POSTGRES_USER:-kevin}"
    db="${POSTGRES_DB:-kevin}"
    password="${POSTGRES_PASSWORD:-}"
    if [ -z "${password}" ] && [ -r "${PG_PASSWORD_FILE}" ]; then
        password="$(cat "${PG_PASSWORD_FILE}")"
    fi
    [ -n "${password}" ] || return 0
    KEVIN__DATABASE__URL="postgres://${user}:${password}@${host}:${port}/${db}"
    export KEVIN__DATABASE__URL
    log "composed KEVIN__DATABASE__URL for ${user}@${host}:${port}/${db}"
}

# --- 3. required inputs ----------------------------------------------------
require_inputs() {
    token_file="${KOHRAL_RUNTIME_TOKEN_FILE:-${KEVIN__KOHRAL__TOKEN_FILE:-/run/secrets/kohral-runtime-token}}"
    if [ ! -r "${token_file}" ]; then
        die "${EXIT_CONFIG}" "the Kohral runtime token is missing: ${token_file} is not readable.
  Kohral binds it as the secret KEVIN_RUNTIME_TOKEN -> API_SERVER_KEY; mount it
  at that path, or point kohral.token_file / KOHRAL_RUNTIME_TOKEN_FILE elsewhere."
    fi
    if [ ! -s "${token_file}" ]; then
        die "${EXIT_CONFIG}" "the Kohral runtime token file ${token_file} is empty; every /v1 endpoint would reject Kohral with 401."
    fi
    if [ -z "${KEVIN__DATABASE__URL_FILE:-}" ] && [ -z "${KEVIN__DATABASE__URL:-}" ]; then
        die "${EXIT_CONFIG}" "no database: mount the URL at ${DATABASE_URL_SECRET} (Kohral's kevin-database-url secret),
  set KEVIN__DATABASE__URL_FILE or KEVIN__DATABASE__URL, or provide POSTGRES_HOST/USER/DB plus ${PG_PASSWORD_FILE}.
  The Kohral stack runs Postgres as the 'memory' service (deploy/kohral/compose.yml)."
    fi
    if [ ! -d "${CONFIG_DIR}" ]; then
        log "warning: ${CONFIG_DIR} does not exist; no operator overlay, no SOUL.md, no platform documentation"
    fi
}

# `--config` must point at a file that exists, so only export KEVIN_CONFIG when
# Kohral actually mounted the overlay.
select_config_file() {
    [ -z "${KEVIN_CONFIG:-}" ] || return 0
    if [ -r "${CONFIG_DIR}/kevin.toml" ]; then
        KEVIN_CONFIG="${CONFIG_DIR}/kevin.toml"
        export KEVIN_CONFIG
        log "using the operator overlay ${KEVIN_CONFIG}"
    fi
}

# --- 4. writable layout ----------------------------------------------------
prepare_data_dir() {
    dir="${1:-${DATA_DIR}}"
    mkdir -p "${dir}" "${dir}/home" "${dir}/work" "${dir}/embeddings" 2>/dev/null || true
    [ -w "${dir}" ] || die "${EXIT_CONFIG}" "${dir} is not writable by uid $(id -u); the Kohral 'data' volume must be owned by 10000:10000."
}

# The image ships the fastembed model; copy it onto the volume once, because
# the cache directory (`<data_dir>/embeddings`) has to stay writable for a
# later model change. Absence is not fatal: memory degrades to lexical search.
seed_model_cache() {
    dir="${1:-${DATA_DIR}}/embeddings"
    cache="${2:-${MODEL_CACHE}}"
    if [ "${KEVIN_SKIP_MODEL_SEED:-0}" = "1" ] || [ ! -d "${cache}" ]; then
        return 0
    fi
    # Already warm? `fastembed` only needs one .onnx anywhere under the cache.
    if find "${dir}" -name '*.onnx' -print -quit 2>/dev/null | grep -q .; then
        return 0
    fi
    find "${cache}" -name '*.onnx' -print -quit 2>/dev/null | grep -q . || return 0
    mkdir -p "${dir}"
    if cp -a "${cache}/." "${dir}/" 2>/dev/null; then
        log "seeded the embedding model cache into ${dir}"
    else
        log "warning: could not seed the embedding model cache into ${dir}"
    fi
}

# The operator API (`server.bind`, inside the stack only) has its own bearer
# token — `plan/08` §1.1 and `serve`: the operator API and the Kohral contract
# never share credentials. Kohral provisions the runtime token; nothing
# provisions this one, so the image mints it once onto the data volume.
ensure_api_token() {
    file="${1:-${KEVIN__SERVER__AUTH_TOKEN_FILE:-${DATA_DIR}/api-token}}"
    if [ -s "${file}" ]; then
        return 0
    fi
    mkdir -p "$(dirname "${file}")" 2>/dev/null || true
    if (umask 077; od -An -N32 -tx1 /dev/urandom | tr -d ' \n' >"${file}"); then
        log "minted the operator API token in ${file}"
    else
        die "${EXIT_CONFIG}" "cannot write the operator API token to ${file}; the data volume must be writable by uid $(id -u)."
    fi
}

# --- 5. migrations ---------------------------------------------------------
# The stack has no ordering guarantee beyond `depends_on`, and `auto_migrate`
# is false in the kohral profile, so migrations are an explicit, retried step.
run_migrations() {
    if [ "${KEVIN_SKIP_MIGRATIONS:-0}" = "1" ]; then
        log "skipping migrations (KEVIN_SKIP_MIGRATIONS=1)"
        return 0
    fi
    deadline=$(( $(date +%s) + MIGRATE_TIMEOUT ))
    attempt=0
    while : ; do
        attempt=$((attempt + 1))
        if kevin db migrate; then
            log "migrations applied (attempt ${attempt})"
            return 0
        fi
        if [ "$(date +%s)" -ge "${deadline}" ]; then
            die "${EXIT_UNREACHABLE}" "database still unreachable after ${MIGRATE_TIMEOUT}s and ${attempt} attempts; check the 'memory' service and KEVIN__DATABASE__URL."
        fi
        log "database not ready yet (attempt ${attempt}); retrying in 2s"
        sleep 2
    done
}

# --- 6. MEMORY.md ----------------------------------------------------------
# Kohral's own reasoning (docs/07 "Platform briefing"): MEMORY.md belongs to the
# agent. Mounting it read-only would break every memory write; rewriting it on
# each rollout would discard what the agent recorded. So: write once, when absent.
memory_seed_text() {
    doc="${1:-${KEVIN__KOHRAL__DOCUMENTATION_FILE:-${CONFIG_DIR}/KOHRAL_DOCUMENTATION.md}}"
    cat <<EOF
# Memory

Kohral hosts and deploys me. Outbound ports, environment variables, credentials,
models and chat channels are operator settings in the Kohral web app, not things
I can alter from inside this container. \`${doc}\` describes each of them and
where the operator finds it — I read that file before answering a question about
how this agent is configured or before saying a change is impossible.
EOF
}

seed_memory_file() {
    file="${1:-${KEVIN__KOHRAL__MEMORY_FILE:-${DATA_DIR}/MEMORY.md}}"
    if [ -e "${file}" ]; then
        log "keeping the agent's own ${file}"
        return 0
    fi
    dir="$(dirname "${file}")"
    mkdir -p "${dir}" 2>/dev/null || true
    if memory_seed_text >"${file}" 2>/dev/null; then
        log "seeded ${file} with the platform-documentation pointer"
    else
        # Seeding is a nicety; a full disk must not be why the gateway never starts.
        log "warning: could not seed ${file}"
    fi
}

# --- main ------------------------------------------------------------------
main() {
    load_env_secret
    resolve_database
    require_inputs
    select_config_file
    prepare_data_dir
    ensure_api_token
    seed_model_cache
    run_migrations
    seed_memory_file
    log "starting: kevin $*"
    exec kevin "$@"
}

if [ "${KEVIN_ENTRYPOINT_LIB:-0}" != "1" ]; then
    main "$@"
fi
