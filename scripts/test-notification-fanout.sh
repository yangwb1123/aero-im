#!/usr/bin/env bash
# test-notification-fanout.sh — DB-gated acceptance gate for the notification
# fan-out integration suite (AT-1…AT-7 of
# docs/design/2026-08-06-aero-im-core-notification-fanout-tests.design.md).
#
# The suite lives in crates/aero-im-core/src/db_tests/ (notifications_tests.rs,
# relay_tests.rs) and drives the production orchestration path
# (dispatch_notifications + both durable relay loops) end-to-end against a real
# Postgres. This script is the enforceable form of design §7 runbook v2:
#
#   * Throwaway DB `aero_test_notif_$$` with the same guardrails as
#     scripts/test-integration.sh: name asserted against ^aero_[A-Za-z0-9_]{1,58}$,
#     never pre-dropped, registered create/drop, EXIT/INT/TERM trap that drops
#     with `DROP DATABASE ... WITH (FORCE)`.
#   * Build BEFORE migrate — migrations are compile-embedded
#     (aero-storage/db.rs sqlx::migrate!("../../migrations")); a stale binary
#     silently no-ops new migrations.
#   * migrate via `cargo run --locked --bin aero-cli -- migrate` with BOTH
#     DATABASE_URL (test harness) and AERO__DATABASE__URL (Figment's
#     AERO__ namespace — aero-cli never reads plain DATABASE_URL) exported.
#   * --locked everywhere; the suite runs with --test-threads=1 (the relay
#     loops claim GLOBAL state — parallel tests would race each other).
#   * Redis presence leg is REQUIRED, never optional: REDIS_URL +
#     AERO__REDIS__URL always exported and pre-flighted. The suite's
#     try_presence_store must skip only when REDIS_URL is UNSET and must fail
#     loudly when set-but-unreachable.
#   * Clean-env precondition: fail closed when AERO_BLOCKED_WORDS,
#     AERO_PII_GUARD, AERO_PII_GUARD_PHONE, or any AERO_AI_* var is set —
#     they flip KeywordModerator::from_env / PiiDetector::from_env / AI
#     fail-open paths and would invalidate the acceptance run.
#
# Usage:
#   bash scripts/test-notification-fanout.sh
#
# Env knobs (DATABASE_URL is a BASE connection string; the script appends the
# throwaway DB name — pass `postgres://user:pass@host:port`, never a URL that
# already names a database):
#   DATABASE_URL — base (default: postgres://aero:aero_dev_pw@localhost:5432)
#   REDIS_URL    — Redis for the presence leg (default: redis://localhost:6379)
#   AERO_POSTGRES_CONTAINER — psql fallback container (default: aero-postgres)

set -euo pipefail

BASE_URL="${DATABASE_URL:-postgres://aero:aero_dev_pw@localhost:5432}"
DB_NAME="aero_test_notif_$$"
REDIS_URL="${REDIS_URL:-redis://localhost:6379}"

# ---------------------------------------------------------------------------
# Guardrail 1 — clean-env precondition (fail-closed; mirrors the exit-2 style
# of test-integration.sh's assert_disposable_db_name).
# ---------------------------------------------------------------------------
assert_clean_env() {
    local offenders=()
    local v
    for v in AERO_BLOCKED_WORDS AERO_PII_GUARD AERO_PII_GUARD_PHONE; do
        if [ -n "${!v:-}" ]; then
            offenders+=("$v")
        fi
    done
    while IFS= read -r v; do
        offenders+=("$v")
    done < <(env | sed -n 's/^\(AERO_AI_[A-Za-z0-9_]*\)=.*/\1/p')
    if [ "${#offenders[@]}" -gt 0 ]; then
        echo "refusing to run with a dirty environment: these vars flip KeywordModerator /" >&2
        echo "PiiDetector / AI fail-open paths and would invalidate the acceptance run:" >&2
        printf '  %s\n' "${offenders[@]}" >&2
        echo "unset them (e.g. unset ${offenders[*]}) and retry" >&2
        exit 2
    fi
}

# ---------------------------------------------------------------------------
# Guardrail 2 — Redis presence leg is REQUIRED (pre-flight; a dead Redis must
# fail the gate, not silently skip AT-4c).
# ---------------------------------------------------------------------------
assert_redis_reachable() {
    local host_port="${REDIS_URL#redis://}"
    host_port="${host_port%%/*}"
    local host="${host_port%%:*}"
    local port="${host_port#*:}"
    [ "$port" = "$host_port" ] && port=6379
    if command -v timeout >/dev/null 2>&1; then
        timeout 3 bash -c "exec 3<>/dev/tcp/${host}/${port}" 2>/dev/null
    else
        bash -c "exec 3<>/dev/tcp/${host}/${port}" 2>/dev/null
    fi || {
        echo "Redis presence leg is REQUIRED: could not connect to ${host}:${port} (REDIS_URL=${REDIS_URL})" >&2
        echo "start it (e.g. docker compose up -d redis) and retry" >&2
        exit 2
    }
    echo "✓ Redis reachable at ${host}:${port} (presence leg will run)"
}

# ---------------------------------------------------------------------------
# Throwaway-DB guardrails — identical semantics to scripts/test-integration.sh:
# assert the name, never pre-drop, register created DBs, refuse to drop
# unregistered ones, trap cleanup with WITH (FORCE).
# ---------------------------------------------------------------------------
assert_disposable_db_name() {
    local variable_name="$1"
    local database_name="$2"
    if [[ ! "$database_name" =~ ^aero_[A-Za-z0-9_]{1,58}$ ]]; then
        echo "${variable_name} must be a dedicated aero_* identifier (letters, digits, underscore)" >&2
        exit 2
    fi
}

assert_disposable_db_name "throwaway database" "$DB_NAME"

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

echo "=== Aero IM — notification fan-out DB-gated suite ==="
echo "Target: postgresql://${PSQL_USER}@${PSQL_HOST}:${PSQL_PORT}/${DB_NAME}"
echo ""

assert_clean_env
assert_redis_reachable

echo "▶ Step 1: build FIRST (migrations are compile-embedded; stale binary = silent no-op)"
cargo build --workspace --locked

echo "▶ Step 2: create throwaway database ${DB_NAME}"
create_throwaway_database "$DB_NAME"

export DATABASE_URL="${BASE_URL}/${DB_NAME}"
export AERO__DATABASE__URL="$DATABASE_URL"
# The test harness reads DATABASE_URL directly, while aero-cli loads database
# configuration through Figment's AERO__DATABASE__URL namespace.
export REDIS_URL
export AERO__REDIS__URL="$REDIS_URL"

echo "▶ Step 3: migrate (after build — ordering is load-bearing)"
cargo run --locked --bin aero-cli -- migrate

echo "▶ Step 4: hermetic gate (proves the new tests stay #[ignore]-gated)"
cargo test --workspace --lib --locked

echo "▶ Step 5: the suite (--test-threads=1 — the relay loops claim global state)"
cargo test -p aero-im-core --lib --locked db_tests:: -- --ignored --test-threads=1

drop_created_database "$DB_NAME"
echo "✓ Notification fan-out suite passed and database dropped"
