#!/usr/bin/env bash
# test-integration.sh — run integration tests against isolated throwaway databases.
#
# Usage:
#   bash scripts/test-integration.sh
#
# What it does:
#   1. Runs each migration regression in its own brand-new throwaway database
#   2. Creates a separate main integration database and runs all migrations
#   3. Runs the remaining ignored library tests against the migrated main database
#   4. Drops every database that this invocation successfully created
#
# Env knobs:
#   DATABASE_URL    — base connection string (default: postgres://aero:aero_dev_pw@localhost:5432)
#   SMOKE_DB        — main integration database name (default: unique aero_integration_<pid>)
#   SKIP_DB_CREATE  — if set, use an existing main DB and skip fresh-DB regressions
#   AERO__NATS__URL — live NATS used by ignored bus tests (default: localhost)
#   REDIS_URL       — live Redis used by ignored cluster-state tests (default: localhost)
#   AERO_POSTGRES_CONTAINER — psql fallback container (default: aero-postgres)

set -euo pipefail

BASE_URL="${DATABASE_URL:-postgres://aero:aero_dev_pw@localhost:5432}"
SMOKE_DB="${SMOKE_DB:-aero_integration_$$}"
SKIP_DB_CREATE="${SKIP_DB_CREATE:-}"
ROLLING_REGRESSION_DB="aero_migration_rolling_$$"
MIGRATION_0192_DB="aero_migration_0192_$$"
BOT_MEMBERSHIP_REGRESSION_DB="aero_migration_0228_$$"

assert_disposable_db_name() {
    local variable_name="$1"
    local database_name="$2"
    if [[ ! "$database_name" =~ ^aero_[A-Za-z0-9_]{1,58}$ ]]; then
        echo "${variable_name} must be a dedicated aero_* identifier (letters, digits, underscore)" >&2
        exit 2
    fi
}

assert_disposable_db_name "SMOKE_DB" "$SMOKE_DB"
assert_disposable_db_name "rolling regression database" "$ROLLING_REGRESSION_DB"
assert_disposable_db_name "0192 regression database" "$MIGRATION_0192_DB"
assert_disposable_db_name "0228 regression database" "$BOT_MEMBERSHIP_REGRESSION_DB"

# Parse host and user from BASE_URL for psql
PSQL_ARGS="${BASE_URL#postgres://}"
PSQL_USER="${PSQL_ARGS%%:*}"
PSQL_REST="${PSQL_ARGS#*:}"
PSQL_PASS="${PSQL_REST%%@*}"
PSQL_HOST_PORT="${PSQL_REST#*@}"
PSQL_HOST="${PSQL_HOST_PORT%:*}"
PSQL_PORT="${PSQL_HOST_PORT#*:}"
PSQL_PORT="${PSQL_PORT%%/*}"

if [[ ! "$PSQL_USER" =~ ^[A-Za-z_][A-Za-z0-9_]{0,62}$ ]]; then
    echo "database user must be a simple PostgreSQL identifier" >&2
    exit 2
fi

export PGPASSWORD="$PSQL_PASS"
PSQL_CONTAINER="${AERO_POSTGRES_CONTAINER:-aero-postgres}"
ACTIVE_CREATED_DB=""

if command -v psql >/dev/null 2>&1; then
    PSQL_MODE="local"
elif command -v docker >/dev/null 2>&1 \
    && docker inspect "$PSQL_CONTAINER" >/dev/null 2>&1; then
    PSQL_MODE="container"
else
    echo "psql is unavailable and PostgreSQL container '$PSQL_CONTAINER' was not found" >&2
    exit 2
fi

run_psql() {
    if [ "$PSQL_MODE" = "local" ]; then
        command psql "$@"
    else
        docker exec -i -e PGPASSWORD="$PGPASSWORD" "$PSQL_CONTAINER" psql "$@"
    fi
}

create_throwaway_database() {
    local database_name="$1"
    assert_disposable_db_name "throwaway database" "$database_name"
    if [ -n "$ACTIVE_CREATED_DB" ]; then
        echo "refusing to create ${database_name}: ${ACTIVE_CREATED_DB} is still registered for cleanup" >&2
        exit 2
    fi

    # Never pre-drop a name. If it already exists, CREATE fails and the script
    # aborts without registering or touching that database.
    run_psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d postgres \
        -v ON_ERROR_STOP=1 \
        -c "CREATE DATABASE \"${database_name}\" OWNER \"${PSQL_USER}\";"
    ACTIVE_CREATED_DB="$database_name"
}

drop_created_database() {
    local database_name="$1"
    if [ "$ACTIVE_CREATED_DB" != "$database_name" ]; then
        echo "refusing to drop unregistered database: ${database_name}" >&2
        exit 2
    fi
    run_psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d postgres \
        -v ON_ERROR_STOP=1 \
        -c "DROP DATABASE IF EXISTS \"${database_name}\" WITH (FORCE);" >/dev/null
    ACTIVE_CREATED_DB=""
}

cleanup() {
    if [ -n "$ACTIVE_CREATED_DB" ]; then
        local database_name="$ACTIVE_CREATED_DB"
        ACTIVE_CREATED_DB=""
        echo "▶ Cleaning up ${database_name}..."
        run_psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d postgres \
            -v ON_ERROR_STOP=1 \
            -c "DROP DATABASE IF EXISTS \"${database_name}\" WITH (FORCE);" >/dev/null 2>&1 || true
        echo "✓ Database dropped"
    fi
}
trap cleanup EXIT INT TERM

run_migration_regression() {
    local database_name="$1"
    local test_name="$2"
    local label="$3"
    local regression_url="${BASE_URL}/${database_name}"

    echo "▶ Creating fresh database for ${label}: ${database_name}"
    create_throwaway_database "$database_name"
    echo "▶ Running ${label}..."
    DATABASE_URL="$regression_url" \
        AERO__DATABASE__URL="$regression_url" \
        cargo test -p aero-storage --lib --locked \
            "$test_name" -- --ignored --test-threads=1
    drop_created_database "$database_name"
    echo "✓ ${label} passed and database dropped"
}

echo "=== Aero IM Integration Tests ==="
echo "Target: postgresql://${PSQL_USER}@${PSQL_HOST}:${PSQL_PORT}/${SMOKE_DB}"
echo ""

# Step 1: migration regressions each own and migrate a distinct empty database.
if [ -z "$SKIP_DB_CREATE" ]; then
    run_migration_regression \
        "$ROLLING_REGRESSION_DB" \
        "rolling_upgrade_fences_are_atomic_before_0176_reasserts_them" \
        "rolling-upgrade migration regression"
    run_migration_regression \
        "$MIGRATION_0192_DB" \
        "migration_0192_repairs_attempted_cross_room_scheduled_replies" \
        "migration 0192 repair regression"
    run_migration_regression \
        "$BOT_MEMBERSHIP_REGRESSION_DB" \
        "migration_0228_backfills_workspace_bot_membership" \
        "migration 0228 bot-membership regression"
else
    echo "▶ SKIP_DB_CREATE set: fresh-DB migration regressions are skipped"
fi

# Step 2: create the separate main integration database only after all
# self-migrating regressions have finished and cleaned up.
if [ -z "$SKIP_DB_CREATE" ]; then
    echo "▶ Creating main integration database: ${SMOKE_DB}"
    create_throwaway_database "$SMOKE_DB"
    echo "✓ Main integration database created"
else
    echo "▶ Using existing main integration database (SKIP_DB_CREATE set)"
fi

FULL_URL="${BASE_URL}/${SMOKE_DB}"
export DATABASE_URL="$FULL_URL"
# The test harness reads DATABASE_URL directly, while aero-cli loads database
# configuration through Figment's AERO__DATABASE__URL namespace.
export AERO__DATABASE__URL="$FULL_URL"
export AERO__NATS__URL="${AERO__NATS__URL:-nats://localhost:4222}"
export REDIS_URL="${REDIS_URL:-redis://localhost:6379}"
export AERO__REDIS__URL="${AERO__REDIS__URL:-$REDIS_URL}"

# Step 3: Run migrations
echo "▶ Running migrations..."
cargo run --bin aero-cli -- migrate 2>&1 | tail -1
echo "✓ Migrations applied"

# Step 4: Run the remaining integration tests. Self-migrating regressions
# are always excluded here because this database is deliberately non-empty.
echo "▶ Running integration tests..."
echo ""
set +e
cargo test --workspace --lib --locked -- \
    --ignored --test-threads=1 \
    --skip eicar_is_reported_infected_against_real_clamd \
    --skip rolling_upgrade_fences_are_atomic_before_0176_reasserts_them \
    --skip migration_0192_repairs_attempted_cross_room_scheduled_replies \
    --skip migration_0228_backfills_workspace_bot_membership
TEST_EXIT=$?
set -e

echo ""
if [ "$TEST_EXIT" -eq 0 ]; then
    echo "✓ All integration tests passed"
else
    echo "✗ Some integration tests failed (exit code: $TEST_EXIT)"
fi
exit "$TEST_EXIT"
