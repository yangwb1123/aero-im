#!/usr/bin/env bash
# test-integration.sh — run all integration tests against a throwaway database.
#
# Usage:
#   bash scripts/test-integration.sh
#
# What it does:
#   1. Creates a temporary PostgreSQL database
#   2. Runs all migrations against it
#   3. Runs `cargo test -- --include-ignored` with DATABASE_URL set
#   4. Drops the temporary database
#
# Env knobs:
#   DATABASE_URL    — base connection string (default: postgres://aero:aero_dev_pw@localhost:5432)
#   SMOKE_DB        — throwaway database name (default: aero_integration_test)
#   SKIP_DB_CREATE  — if set, skip DB creation (use existing DB)

set -euo pipefail

BASE_URL="${DATABASE_URL:-postgres://aero:aero_dev_pw@localhost:5432}"
SMOKE_DB="${SMOKE_DB:-aero_integration_test}"
SKIP_DB_CREATE="${SKIP_DB_CREATE:-}"

# Parse host and user from BASE_URL for psql
PSQL_ARGS="${BASE_URL#postgres://}"
PSQL_USER="${PSQL_ARGS%%:*}"
PSQL_REST="${PSQL_ARGS#*:}"
PSQL_PASS="${PSQL_REST%%@*}"
PSQL_HOST_PORT="${PSQL_REST#*@}"
PSQL_HOST="${PSQL_HOST_PORT%:*}"
PSQL_PORT="${PSQL_HOST_PORT#*:}"
PSQL_PORT="${PSQL_PORT%%/*}"

export PGPASSWORD="$PSQL_PASS"

echo "=== Aero IM Integration Tests ==="
echo "Base URL: ${BASE_URL}/${SMOKE_DB}"
echo ""

# Step 1: Create temporary database
if [ -z "$SKIP_DB_CREATE" ]; then
    echo "▶ Creating database: ${SMOKE_DB}"
    psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d postgres \
        -c "CREATE DATABASE ${SMOKE_DB} OWNER ${PSQL_USER};" 2>/dev/null || {
        echo "  Database already exists, dropping and recreating..."
        psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d postgres \
            -c "DROP DATABASE IF EXISTS ${SMOKE_DB};"
        psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d postgres \
            -c "CREATE DATABASE ${SMOKE_DB} OWNER ${PSQL_USER};"
    }
    echo "✓ Database created"
else
    echo "▶ Using existing database (SKIP_DB_CREATE set)"
fi

FULL_URL="${BASE_URL}/${SMOKE_DB}"
export DATABASE_URL="$FULL_URL"

# Step 2: Run migrations
echo "▶ Running migrations..."
cargo run --bin aero-cli -- migrate 2>&1 | tail -1
echo "✓ Migrations applied"

# Step 3: Run integration tests
echo "▶ Running integration tests..."
echo ""
cargo test --workspace -- --include-ignored 2>&1
TEST_EXIT=$?

# Step 4: Drop temporary database
if [ -z "$SKIP_DB_CREATE" ]; then
    echo "▶ Cleaning up..."
    psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d postgres \
        -c "DROP DATABASE IF EXISTS ${SMOKE_DB};" 2>/dev/null || true
    echo "✓ Database dropped"
fi

echo ""
if [ "$TEST_EXIT" -eq 0 ]; then
    echo "✓ All integration tests passed"
else
    echo "✗ Some integration tests failed (exit code: $TEST_EXIT)"
fi
exit "$TEST_EXIT"
