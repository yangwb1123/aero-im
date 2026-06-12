#!/usr/bin/env bash
#
# migrate_chain_smoke.sh — prove the WHOLE migration chain applies on a fresh DB.
#
# Why: migrations are compile-embedded via `sqlx::migrate!` and only ever applied
# to long-lived dev/prod DBs, so a migration that breaks a *fresh* deploy is never
# caught (e.g. 0109 once did `ADD COLUMN tier_id` that 0051 already defined — the
# chain died with "column already exists", but no existing DB re-ran 0109). This
# script replays every migrations/*.sql, in order, against a brand-new throwaway
# database and fails loudly on the first error. Run it in CI and before release.
#
# Usage:
#   # CI (a postgres service with host `psql`):
#   PGHOST=localhost PGUSER=aero PGPASSWORD=aero_dev_pw scripts/migrate_chain_smoke.sh
#
#   # Local docker dev stack (no host psql — go through the container):
#   PSQL="docker exec -i aero-postgres psql -U aero" scripts/migrate_chain_smoke.sh
#
# Env knobs: PSQL (psql invocation prefix), SMOKE_DB (throwaway db name),
#   ADMIN_DB (db to issue CREATE/DROP from), PGUSER (owner of the throwaway db).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MIG_DIR="$ROOT/migrations"

PSQL="${PSQL:-psql -U ${PGUSER:-aero}}"
ADMIN_DB="${ADMIN_DB:-postgres}"
SMOKE_DB="${SMOKE_DB:-aero_migrate_smoke}"
OWNER="${PGUSER:-aero}"

admin() { $PSQL -d "$ADMIN_DB" -v ON_ERROR_STOP=1 -q "$@"; }
target() { $PSQL -d "$SMOKE_DB" -v ON_ERROR_STOP=1 -q "$@"; }

cleanup() { admin -c "DROP DATABASE IF EXISTS $SMOKE_DB WITH (FORCE);" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "==> (re)creating throwaway database $SMOKE_DB"
admin -c "DROP DATABASE IF EXISTS $SMOKE_DB WITH (FORCE);"
admin -c "CREATE DATABASE $SMOKE_DB OWNER $OWNER;"

total="$(find "$MIG_DIR" -maxdepth 1 -name '*.sql' | wc -l | tr -d ' ')"
echo "==> replaying $total migrations from $MIG_DIR"

n=0
for f in "$MIG_DIR"/*.sql; do
  if ! target < "$f"; then
    echo "FAIL: migration $(basename "$f") did not apply on a fresh database" >&2
    echo "      (applied $n/$total before failing)" >&2
    exit 1
  fi
  n=$((n + 1))
done

echo "==> OK: all $n/$total migrations applied cleanly on a fresh database"
