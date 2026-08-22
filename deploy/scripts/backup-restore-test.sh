#!/usr/bin/env bash
# Exercises Kevin's backup and restore procedure end to end
# (`plan/10-observability-ops.md` §Migrations and data).
#
#   dump → restore into a scratch database → compare row counts →
#   rebuild every projection → compare again → drop the scratch database
#
# Postgres is the only source of truth: `data_dir` holds transcripts, artifact
# copies and the embedding-model cache, all rebuildable or re-downloadable.
# That is why this script backs up the database and nothing else, and why the
# check that matters is "can the read models be rebuilt from the restored
# events", not "are the files identical".
#
# Usage:
#   deploy/scripts/backup-restore-test.sh [--keep] [--dump-file PATH]
#
# Environment:
#   DATABASE_URL   source database (default postgres://kevin:kevin@localhost:5433/kevin)
#   KEVIN_BIN      the kevin binary (default: `kevin` on PATH, else target/debug/kevin)
#
# Exit codes: 0 identical after restore, 1 a mismatch or a failed step.

set -Eeuo pipefail

DATABASE_URL="${DATABASE_URL:-postgres://kevin:kevin@localhost:5433/kevin}"
KEEP=0
DUMP_FILE=""

while [ $# -gt 0 ]; do
  case "$1" in
    --keep) KEEP=1 ;;
    --dump-file) DUMP_FILE="${2:?--dump-file needs a path}"; shift ;;
    -h|--help) sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
  shift
done

for tool in pg_dump pg_restore psql; do
  command -v "$tool" >/dev/null || { echo "missing $tool (install postgresql-client)" >&2; exit 1; }
done

KEVIN_BIN="${KEVIN_BIN:-$(command -v kevin || true)}"
if [ -z "$KEVIN_BIN" ] && [ -x "target/debug/kevin" ]; then KEVIN_BIN="target/debug/kevin"; fi
[ -n "$KEVIN_BIN" ] || { echo "no kevin binary: set KEVIN_BIN or cargo build" >&2; exit 1; }

WORK="$(mktemp -d)"
[ -n "$DUMP_FILE" ] || DUMP_FILE="$WORK/kevin.dump"
SCRATCH="kevin_restore_test_$$"
# The maintenance database CREATE/DROP DATABASE run against.
ADMIN_URL="${DATABASE_URL%/*}/postgres"
SCRATCH_URL="${DATABASE_URL%/*}/$SCRATCH"

cleanup() {
  if [ "$KEEP" -eq 0 ]; then
    psql "$ADMIN_URL" -qtAX -c "DROP DATABASE IF EXISTS $SCRATCH WITH (FORCE)" >/dev/null 2>&1 || true
    rm -rf "$WORK"
  else
    echo "kept: dump=$DUMP_FILE scratch=$SCRATCH_URL"
  fi
}
trap cleanup EXIT

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }

# The tables whose contents must survive a restore. `core.events` is the
# authority; the `orch.*` read models are derived and are rebuilt below.
COUNT_SQL="
  SELECT 'core.events', count(*) FROM core.events
  UNION ALL SELECT 'core.outbox', count(*) FROM core.outbox
  UNION ALL SELECT 'core.processed_commands', count(*) FROM core.processed_commands
  UNION ALL SELECT 'orch.run_overview', count(*) FROM orch.run_overview
  UNION ALL SELECT 'orch.task_board', count(*) FROM orch.task_board
  UNION ALL SELECT 'orch.question_inbox', count(*) FROM orch.question_inbox
  UNION ALL SELECT 'orch.cost_ledger', count(*) FROM orch.cost_ledger
  UNION ALL SELECT 'memory.memory_items', count(*) FROM memory.memory_items
  ORDER BY 1"

counts() { psql "$1" -qtAX -F'=' -c "$COUNT_SQL"; }

say "source: ${DATABASE_URL%%\?*}"
counts "$DATABASE_URL" > "$WORK/before.txt"
cat "$WORK/before.txt"

say "dump → $DUMP_FILE"
pg_dump --format=custom --no-owner --no-privileges --file="$DUMP_FILE" "$DATABASE_URL"

say "restore → $SCRATCH"
psql "$ADMIN_URL" -qtAX -c "DROP DATABASE IF EXISTS $SCRATCH WITH (FORCE)" >/dev/null
psql "$ADMIN_URL" -qtAX -c "CREATE DATABASE $SCRATCH" >/dev/null
# pgvector must exist before the restore recreates the vector columns.
psql "$SCRATCH_URL" -qtAX -c "CREATE EXTENSION IF NOT EXISTS vector" >/dev/null
pg_restore --no-owner --no-privileges --dbname="$SCRATCH_URL" "$DUMP_FILE"

say "kevin db status (restored)"
KEVIN__DATABASE__URL="$SCRATCH_URL" "$KEVIN_BIN" db status

say "integrity: row counts after the restore"
counts "$SCRATCH_URL" > "$WORK/restored.txt"
if ! diff -u "$WORK/before.txt" "$WORK/restored.txt"; then
  echo "FAIL: the restored database does not match the source" >&2
  exit 1
fi

say "kevin db rebuild-projection --all"
KEVIN__DATABASE__URL="$SCRATCH_URL" "$KEVIN_BIN" db rebuild-projection --all

say "integrity: the read models rebuild to the same rows"
counts "$SCRATCH_URL" > "$WORK/rebuilt.txt"
if ! diff -u "$WORK/before.txt" "$WORK/rebuilt.txt"; then
  echo "FAIL: rebuilding the projections changed the read models" >&2
  echo "      (the events restored fine; a projection is not deterministic)" >&2
  exit 1
fi

say "OK — dump, restore and projection rebuild are consistent"
