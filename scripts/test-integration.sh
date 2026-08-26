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
INTEGRATION_USER_ISSUER_REGRESSION_DB="aero_migration_0233_$$"
SNAPLINK_CREDENTIAL_REGRESSION_DB="aero_migration_0237_$$"
SNAPLINK_COMMERCIAL_INTEGRATION_DB="aero_snaplink_commercial_$$"
SCIM_NIL_WORKSPACE_INTEGRATION_DB="aero_scim_nil_workspace_$$"
AUDIT_CONNECTOR_INTEGRATION_DB="aero_audit_connector_$$"
AUDIT_GOVERNANCE_INTEGRATION_DB="aero_audit_governance_$$"
MODERATION_FINALIZE_PARITY_INTEGRATION_DB="aero_moderation_finalize_parity_$$"
AUTH_OUTBOX_PARITY_INTEGRATION_DB="aero_auth_outbox_parity_$$"
MODERATION_FINALIZE_DRILL_DB="aero_moderation_finalize_drill_$$"
T11_DRILL_DB="aero_t11_drill_$$"
PRIORITY_DRILL_DB="aero_priority_drill_$$"
AUDIT_PROVISION_DB="aero_audit_provision_$$"
L1_AGGREGATE_DB="aero_l1_aggregate_$$"
L1_PARITY_DB="aero_l1_parity_$$"
ROOM_LANE_DB="aero_room_lane_$$"
MESSAGE_LANE_DB="aero_message_lane_$$"
FACADE_L1_WINDOW_DB="aero_facade_l1_window_$$"
ROOM_FACADE_DRILL_DB="aero_room_facade_drill_$$"
RECALL_FACADE_DRILL_DB="aero_recall_facade_drill_$$"
POSTURE_DRILL_DB="aero_posture_drill_$$"
R4_SINK_LEG_DB="aero_r4_sink_leg_$$"
AUDIT_OUTBOX_SAMPLER_DB="aero_audit_sampler_$$"

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
assert_disposable_db_name "0233 regression database" "$INTEGRATION_USER_ISSUER_REGRESSION_DB"
assert_disposable_db_name "0237 regression database" "$SNAPLINK_CREDENTIAL_REGRESSION_DB"
assert_disposable_db_name "Snaplink commercial integration database" "$SNAPLINK_COMMERCIAL_INTEGRATION_DB"
assert_disposable_db_name "SCIM nil-workspace integration database" "$SCIM_NIL_WORKSPACE_INTEGRATION_DB"
assert_disposable_db_name "audit connector integration database" "$AUDIT_CONNECTOR_INTEGRATION_DB"
assert_disposable_db_name "audit governance integration database" "$AUDIT_GOVERNANCE_INTEGRATION_DB"
assert_disposable_db_name "moderation finalize parity integration database" "$MODERATION_FINALIZE_PARITY_INTEGRATION_DB"
assert_disposable_db_name "auth outbox parity integration database" "$AUTH_OUTBOX_PARITY_INTEGRATION_DB"
assert_disposable_db_name "moderation finalize drill database" "$MODERATION_FINALIZE_DRILL_DB"
assert_disposable_db_name "T-11 fail-closed drill database" "$T11_DRILL_DB"
assert_disposable_db_name "moderation priority drill database" "$PRIORITY_DRILL_DB"
assert_disposable_db_name "audit provision leg B database" "$AUDIT_PROVISION_DB"
assert_disposable_db_name "L1 window aggregate database" "$L1_AGGREGATE_DB"
assert_disposable_db_name "L1 parity drill database" "$L1_PARITY_DB"
assert_disposable_db_name "room lane parity database" "$ROOM_LANE_DB"
assert_disposable_db_name "message lane parity database" "$MESSAGE_LANE_DB"
assert_disposable_db_name "facade L1 window drill database" "$FACADE_L1_WINDOW_DB"
assert_disposable_db_name "room facade drill database" "$ROOM_FACADE_DRILL_DB"
assert_disposable_db_name "recall facade drill database" "$RECALL_FACADE_DRILL_DB"
assert_disposable_db_name "connector posture drill database" "$POSTURE_DRILL_DB"
assert_disposable_db_name "R4 sink leg drill database" "$R4_SINK_LEG_DB"
assert_disposable_db_name "audit outbox sampler database" "$AUDIT_OUTBOX_SAMPLER_DB"

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
SAMPLER_BINDINGS_FILE=""

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

# B5_LOG is script-owned: initialize it BEFORE the trap so an exit in the
# pre-assignment window (e.g. `source b5-pin.sh` failing, or mktemp failing
# under an adversarial TMPDIR) can never make cleanup rm an env-provided path.
B5_LOG=""
cleanup() {
    if [ -n "${B5_LOG:-}" ]; then
        rm -f "$B5_LOG"
        B5_LOG=""
    fi
    if [ -n "${SAMPLER_BINDINGS_FILE:-}" ]; then
        rm -f "$SAMPLER_BINDINGS_FILE"
        SAMPLER_BINDINGS_FILE=""
    fi
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

# B5 acceptance gate (G6 总装门): load the contract-pin material
# (scripts/b5-pin.sh) and self-test the guard first — pure bash, instant — so
# a pin-guard regression fails the harness before the expensive DB work.
# shellcheck disable=SC1091
source "$(dirname "${BASH_SOURCE[0]}")/b5-pin.sh"
B5_LOG="$(mktemp "${TMPDIR:-/tmp}/aero-b5-log.XXXXXX")"
bash "$(dirname "${BASH_SOURCE[0]}")/test-b5-pin-guard.sh"

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
    b5_check "${2}" "PASS"
}

run_migrated_integration() {
    local database_name="$1"
    local test_name="$2"
    local label="$3"
    local crate_name="${4:-aero-storage}"
    # Optional 5th arg: the B5 slot name for the verdict line (defaults to
    # the test filter). Needed when the slot name and the cargo filter
    # differ (e.g. slot facade-l1-window-drill runs the drill_facade_ filter) —
    # the pin guard greps B5_LOG for the SLOT name.
    local slot_name="${5:-$test_name}"
    local integration_url="${BASE_URL}/${database_name}"
    local test_output=""

    echo "▶ Creating fresh database for ${label}: ${database_name}"
    create_throwaway_database "$database_name"
    echo "▶ Migrating database for ${label}..."
    DATABASE_URL="$integration_url" \
        AERO__DATABASE__URL="$integration_url" \
        cargo run --bin aero-cli -- migrate 2>&1 | tail -1
    echo "▶ Running ${label}..."
    if test_output=$(DATABASE_URL="$integration_url" \
            AERO__DATABASE__URL="$integration_url" \
            cargo test -p "$crate_name" --lib --locked \
                "$test_name" -- --ignored --test-threads=1 2>&1); then
        :
    else
        test_status=$?
        echo "$test_output"
        drop_created_database "$database_name"
        echo "✗ ${label} FAILED (cargo test exited ${test_status})" >&2
        exit 1
    fi
    echo "$test_output"
    # Empty-filter guard (B5-1 A1.3): a non-matching test filter exits 0 with
    # "running 0 tests … 0 passed" — a named entry would be green before any
    # test exists. Fail the entry unless ≥1 test actually ran and passed.
    if ! grep -Eq 'test result: ok\. [1-9][0-9]* passed' <<<"$test_output"; then
        drop_created_database "$database_name"
        echo "✗ ${label}: no test matched (empty-filter guard) — the filter must match ≥1 existing test" >&2
        exit 1
    fi
    drop_created_database "$database_name"
    echo "✓ ${label} passed and database dropped"
    b5_check "${slot_name}" "PASS"
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
    run_migration_regression \
        "$INTEGRATION_USER_ISSUER_REGRESSION_DB" \
        "migration_0233_backfills_and_constrains_human_identity_issuer" \
        "migration 0233 integration-user-issuer regression"
    run_migration_regression \
        "$SNAPLINK_CREDENTIAL_REGRESSION_DB" \
        "migration_0237_backfills_before_installing_destination_guard" \
        "migration 0237 Snaplink credential regression"
    run_migrated_integration \
        "$SNAPLINK_COMMERCIAL_INTEGRATION_DB" \
        "message_quota_and_snaplink_outboxes_are_transactional" \
        "Snaplink commercial quota and outbox integration"
    run_migrated_integration \
        "$SCIM_NIL_WORKSPACE_INTEGRATION_DB" \
        "scim_inactive_first_nil_workspace_member_rolls_back_owner_bootstrap" \
        "SCIM dormant nil-workspace regression"
    # B5-4 provisioning seam (leg B, always-run, v1): throwaway DB → migrate
    # → fresh state must be `consistent` (relay off + zero undelivered) →
    # seed one v1 audit row → fail-closed with the no-grant reason (A1).
    # Reuses the pinned audit-provision-check slot (count-driven); the
    # verdict greps make a stale/no-op command fail red instead of passing.
    B5_HELP_OUT="$(cargo run -p aero-cli -- help 2>&1 || true)"
    if grep -q "audit-provision-check" <<<"$B5_HELP_OUT"; then
        echo "▶ Creating fresh database for audit-provision-check leg B: ${AUDIT_PROVISION_DB}"
        create_throwaway_database "$AUDIT_PROVISION_DB"
        AUDIT_PROVISION_URL="${BASE_URL}/${AUDIT_PROVISION_DB}"
        echo "▶ Migrating database for audit-provision-check leg B..."
        DATABASE_URL="$AUDIT_PROVISION_URL" \
            AERO__DATABASE__URL="$AUDIT_PROVISION_URL" \
            cargo run --bin aero-cli -- migrate 2>&1 | tail -1
        echo "▶ Running audit-provision-check leg B1 (empty DB ⇒ consistent)..."
        if B1_OUT="$(DATABASE_URL="$AUDIT_PROVISION_URL" \
                AERO__DATABASE__URL="$AUDIT_PROVISION_URL" \
                cargo run -p aero-cli -- audit-provision-check 2>&1)"; then
            :
        else
            echo "✗ audit-provision-check leg B1: expected exit 0 on an empty migrated DB" >&2
            echo "$B1_OUT" >&2
            drop_created_database "$AUDIT_PROVISION_DB"
            exit 1
        fi
        if ! grep -q "verdict: consistent" <<<"$B1_OUT"; then
            echo "✗ audit-provision-check leg B1: expected 'verdict: consistent' on an empty DB" >&2
            echo "$B1_OUT" >&2
            drop_created_database "$AUDIT_PROVISION_DB"
            exit 1
        fi
        echo "✓ audit-provision-check leg B1: consistent on empty DB"
        echo "▶ Running audit-provision-check leg B2 (one undelivered v1 audit row ⇒ fail-closed)..."
        run_psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d "$AUDIT_PROVISION_DB" \
            -v ON_ERROR_STOP=1 \
            -c "INSERT INTO snaplink_delivery_outbox (delivery_id, destination, workspace_id, tenant_id, client_id, source_system, idempotency_key, payload, occurred_at) VALUES ('audit:b5-leg-b', 'audit', gen_random_uuid(), 'tenant-b5b', 'client-b5b', 'aero-im.source', 'audit:b5-leg-b', '{\"event_id\":\"00000000-0000-0000-0000-000000000001\"}'::jsonb, clock_timestamp());"
        if B2_OUT="$(DATABASE_URL="$AUDIT_PROVISION_URL" \
                AERO__DATABASE__URL="$AUDIT_PROVISION_URL" \
                cargo run -p aero-cli -- audit-provision-check 2>&1)"; then
            echo "✗ audit-provision-check leg B2: expected non-zero exit with an undelivered audit row (fail-closed)" >&2
            echo "$B2_OUT" >&2
            drop_created_database "$AUDIT_PROVISION_DB"
            exit 1
        fi
        if ! grep -q "verdict: fail-closed" <<<"$B2_OUT" \
            || ! grep -q "no audit:event:write grant issued" <<<"$B2_OUT"; then
            echo "✗ audit-provision-check leg B2: expected 'verdict: fail-closed' with the no-grant reason" >&2
            echo "$B2_OUT" >&2
            drop_created_database "$AUDIT_PROVISION_DB"
            exit 1
        fi
        echo "✓ audit-provision-check leg B2: fail-closed on undelivered audit row"
        drop_created_database "$AUDIT_PROVISION_DB"
        b5_check "audit-provision-check" "PASS"
    else
        b5_check "audit-provision-check" "SKIP (B5-4 audit-provision-check not landed)"
    fi
    # B5-1 (aero-ai slice): named governance-parity entries — each on its own
    # throwaway migrated DB (--test-threads=1), gated on the B5-1 storage
    # slice's 0239 migration (AUDIT_CONNECTOR precedent). While the table is
    # absent the entries SKIP; once it lands each must match ≥1 existing test —
    # the run_migrated_integration empty-filter guard fails the entry
    # otherwise (A1.3: no vacuous green).
    if [ -f "migrations/0239_audit_governance_outbox.sql" ]; then
        run_migrated_integration \
            "$AUDIT_GOVERNANCE_INTEGRATION_DB" \
            "audit_governance::" \
            "audit governance outbox db_tests"
        run_migrated_integration \
            "$MODERATION_FINALIZE_PARITY_INTEGRATION_DB" \
            "moderation_finalize_outbox_parity" \
            "moderation finalize outbox parity"
        run_migrated_integration \
            "$MODERATION_FINALIZE_DRILL_DB" \
            "moderation_finalize_drill" \
            "ai-worker moderation finalize drill db_tests" \
            "aero-ai"
        # B5-1 auth slice: the named §2.7 parity entry (auth_outbox_parity_1to1
        # — the empty-filter guard makes the name load-bearing; a vacuous green
        # is a FAIL).
        run_migrated_integration \
            "$AUTH_OUTBOX_PARITY_INTEGRATION_DB" \
            "auth_outbox_parity" \
            "auth outbox parity 1:1"
    else
        echo "▶ Skipping B5-1 governance entries: migrations/0239_audit_governance_outbox.sql (B5-1 storage slice) has not landed"
        b5_check "audit_governance::" "SKIP (0239 not landed)"
        b5_check "moderation_finalize_outbox_parity" "SKIP (0239 not landed)"
        b5_check "moderation_finalize_drill" "SKIP (0239 not landed)"
        b5_check "auth_outbox_parity" "SKIP (0239 not landed)"
    fi
    # A3 relay drill (B5-2): throwaway DB → migrate → seed N governance rows →
    # run the connector relay against a stub audit sink → assert
    # COUNT(status=2) == N + event_id set-parity. Gated on B5-1's 0239
    # migration (the drill exits 2 with a clear SKIP when the table is absent,
    # so this section stays green during the phase-1 window).
    if [ -f "migrations/0239_audit_governance_outbox.sql" ]; then
        echo "▶ Creating fresh database for audit connector drill: ${AUDIT_CONNECTOR_INTEGRATION_DB}"
        create_throwaway_database "$AUDIT_CONNECTOR_INTEGRATION_DB"
        AUDIT_DRILL_URL="${BASE_URL}/${AUDIT_CONNECTOR_INTEGRATION_DB}"
        echo "▶ Migrating database for audit connector drill..."
        DATABASE_URL="$AUDIT_DRILL_URL" \
            AERO__DATABASE__URL="$AUDIT_DRILL_URL" \
            cargo run --bin aero-cli -- migrate 2>&1 | tail -1
        echo "▶ Running audit connector relay drill (A3)..."
        DATABASE_URL="$AUDIT_DRILL_URL" \
            cargo run --quiet --locked -p aero-audit-connector --bin aero-audit-relay-drill
        drop_created_database "$AUDIT_CONNECTOR_INTEGRATION_DB"
        echo "✓ Audit connector drill passed (status 2 == N, event_id parity)"
        b5_check "a3-relay-drill" "PASS"
    else
        echo "▶ Skipping audit connector drill: migrations/0239_audit_governance_outbox.sql (B5-1) has not landed"
        b5_check "a3-relay-drill" "SKIP (0239 not landed)"
    fi
    # T-11 fail-closed drill (B5 acceptance, AC2): relay absent (token
    # endpoint = deterministically closed loopback port) ⇒ seeded rows must
    # stay pending (status 0, zero terminal states, per-round attempts
    # evidence, last_error records the transport failure) — never silently
    # delivered, never falsely dead. The new aero-audit-t11-drill bin exits 2
    # when the 0239 table is absent; the section is gated on the migration
    # file (A3 precedent) and stays green with an explicit SKIP verdict line
    # during the phase-1 window.
    if [ -f "migrations/0239_audit_governance_outbox.sql" ]; then
        echo "▶ Creating fresh database for T-11 fail-closed drill: ${T11_DRILL_DB}"
        create_throwaway_database "$T11_DRILL_DB"
        T11_DRILL_URL="${BASE_URL}/${T11_DRILL_DB}"
        echo "▶ Migrating database for T-11 fail-closed drill..."
        DATABASE_URL="$T11_DRILL_URL" \
            AERO__DATABASE__URL="$T11_DRILL_URL" \
            cargo run --bin aero-cli -- migrate 2>&1 | tail -1
        echo "▶ Running T-11 fail-closed drill (relay absent ⇒ rows stay pending)..."
        DATABASE_URL="$T11_DRILL_URL" \
            cargo run --quiet --locked -p aero-audit-connector --bin aero-audit-t11-drill
        # B5-4 provisioning seam (T-11 leg 2): when the sibling's
        # audit-provision-check command has landed, the same relay-absent DB
        # must refuse any audit:event:write grant (fail-closed) — non-zero
        # exit. Detection = help output contains the command name (guards
        # against "unknown command" also exiting non-zero being misread as
        # PASS). Seam not landed → explicit SKIP verdict (non-vacuous, same
        # 0239 file-gate precedent).
        B5_HELP_OUT="$(cargo run -p aero-cli -- help 2>&1 || true)"
        if grep -q "audit-provision-check" <<<"$B5_HELP_OUT"; then
            if DATABASE_URL="$T11_DRILL_URL" \
                AERO__DATABASE__URL="$T11_DRILL_URL" \
                cargo run -p aero-cli -- audit-provision-check >/dev/null 2>&1; then
                echo "✗ B5 provisioning leg: audit-provision-check exited 0 on a relay-absent DB (must be fail-closed)" >&2
                exit 1
            fi
            echo "✓ audit-provision-check refused the grant (fail-closed)"
            b5_check "audit-provision-check" "PASS"
        else
            b5_check "audit-provision-check" "SKIP (B5-4 audit-provision-check not landed)"
        fi
        # B5-4 provisioning seam (legs D/C, 0239-gated, on the T-11 DB before
        # drop): relay enabled + one binding ⇒ healthy verdict with the 0239
        # distribution and oldest-pending age (A2); then one row forced to
        # status 3 (simulated 403/provisioning refusal) ⇒ the check reports
        # it as dead — never folded into delivered — and stays fail-closed
        # (A3). Reuses the pinned audit-provision-check slot.
        if grep -q "audit-provision-check" <<<"$B5_HELP_OUT"; then
            echo "▶ Running audit-provision-check leg D (relay enabled ⇒ healthy)..."
            run_psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d "$T11_DRILL_DB" \
                -v ON_ERROR_STOP=1 \
                -c "UPDATE snaplink_commercial_runtime SET enabled = TRUE, updated_at = clock_timestamp() WHERE singleton;"
            run_psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d "$T11_DRILL_DB" \
                -v ON_ERROR_STOP=1 \
                -c "INSERT INTO snaplink_commercial_bindings (workspace_id, tenant_id, client_id, audit_client_id, source_system, revision, enabled) VALUES (gen_random_uuid(), 'tenant-b5d', 'client-b5d', 'audit-client-b5d', 'aero-im.source', 1, TRUE);"
            if D_OUT="$(DATABASE_URL="$T11_DRILL_URL" \
                    AERO__DATABASE__URL="$T11_DRILL_URL" \
                    cargo run -p aero-cli -- audit-provision-check 2>&1)"; then
                :
            else
                echo "✗ audit-provision-check leg D: expected exit 0 with relay enabled (healthy)" >&2
                echo "$D_OUT" >&2
                exit 1
            fi
            if ! grep -q "verdict: healthy" <<<"$D_OUT" \
                || ! grep -Eq "outbox-0239: table=audit_governance_outbox pending=[0-9]+ claimed=0 delivered=0 dead=0" <<<"$D_OUT" \
                || ! grep -q "oldest-pending-age:" <<<"$D_OUT"; then
                echo "✗ audit-provision-check leg D: expected healthy verdict + 0239 distribution + oldest-pending-age" >&2
                echo "$D_OUT" >&2
                exit 1
            fi
            echo "✓ audit-provision-check leg D: healthy with 0239 distribution"
            # B5-4 runtime-health leg F′: a zero-output stub that exits 0 must
            # still fail the --relay gate.  This proves the captured-output
            # validator does not trust a process exit code by itself.
            echo "▶ Running audit-provision-check leg F′ (--relay stub negative)..."
            set +e
            F_PRIME_OUT="$(AERO_RELAY_PROBE_BIN=/bin/true \
                DATABASE_URL="$T11_DRILL_URL" \
                AERO__DATABASE__URL="$T11_DRILL_URL" \
                cargo run --quiet -p aero-cli -- audit-provision-check --relay 2>&1)"
            F_PRIME_RC=$?
            set -e
            if [ "$F_PRIME_RC" -eq 0 ] || ! grep -q "relay-probe: FAIL" <<<"$F_PRIME_OUT"; then
                echo "✗ audit-provision-check leg F′: expected non-zero + relay-probe: FAIL" >&2
                echo "$F_PRIME_OUT" >&2
                exit 1
            fi
            echo "✓ audit-provision-check leg F′: --relay rejected zero-output stub"
            b5_check "audit-provision-check" "PASS"

            # B5-4 runtime-health leg F: the real DB-free probe must be part
            # of the healthy provisioning verdict.  Keep the file gate as a
            # defensive phase-1 guard; once landed, a missing/partial probe
            # is a red failure rather than a silent healthy result.
            echo "▶ Running audit-provision-check leg F (--relay real probe)..."
            if [ -f "crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs" ]; then
                if F_OUT="$(DATABASE_URL="$T11_DRILL_URL" \
                        AERO__DATABASE__URL="$T11_DRILL_URL" \
                        cargo run --quiet -p aero-cli -- audit-provision-check --relay 2>&1)"; then
                    :
                else
                    echo "✗ audit-provision-check leg F: expected --relay exit 0" >&2
                    echo "$F_OUT" >&2
                    exit 1
                fi
                if ! grep -q "audit-provision-check: relay-probe: PASS (9/9)" <<<"$F_OUT" \
                    || [ "$(grep -c '^probe: .*: PASS$' <<<"$F_OUT")" -ne 9 ]; then
                    echo "✗ audit-provision-check leg F: expected 9 probe PASS lines" >&2
                    echo "$F_OUT" >&2
                    exit 1
                fi
                echo "$F_OUT"
                echo "✓ audit-provision-check leg F: --relay healthy with 9/9 probe PASS"
                b5_check "audit-provision-check" "PASS"
            else
                echo "▶ audit-provision-check leg F SKIP: B5-2 relay probe not landed"
                b5_check "audit-provision-check" "SKIP (B5-2 relay probe not landed)"
            fi

            # B5-4 runtime-health leg E: a claimed row older than the relay's
            # maximum lease is a fail-closed stuck-lease signal.  The claim
            # token and lease columns are both populated to satisfy 0239's
            # claim-state CHECK constraint.
            echo "▶ Running audit-provision-check leg E (stuck lease)..."
            run_psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d "$T11_DRILL_DB" \
                -v ON_ERROR_STOP=1 \
                -c "INSERT INTO audit_governance_outbox (event_id, status, class, priority, payload, available_at, claim_token, lease_expires_at) VALUES (gen_random_uuid(), 1, 'message', 10, jsonb_build_object('event_id', gen_random_uuid()::text, 'source_system', 'aero-im.source'), clock_timestamp() - interval '2 days', gen_random_uuid(), clock_timestamp() - interval '1 hour');"
            set +e
            E_OUT="$(DATABASE_URL="$T11_DRILL_URL" \
                AERO__DATABASE__URL="$T11_DRILL_URL" \
                cargo run --quiet -p aero-cli -- audit-provision-check 2>&1)"
            E_RC=$?
            set -e
            if [ "$E_RC" -eq 0 ] \
                || ! grep -Eq "audit-provision-check: oldest-claimed-age: [0-9]{6,}s" <<<"$E_OUT" \
                || ! grep -q "stuck lease" <<<"$E_OUT" \
                || ! grep -q "verdict: fail-closed" <<<"$E_OUT"; then
                echo "✗ audit-provision-check leg E: expected stuck-lease fail-closed output" >&2
                echo "$E_OUT" >&2
                exit 1
            fi
            echo "$E_OUT"
            echo "✓ audit-provision-check leg E: stuck lease fail-closed"
            b5_check "audit-provision-check" "PASS"

            echo "▶ Running audit-provision-check leg C (one dead row ⇒ fail-closed, never delivered)..."
            run_psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d "$T11_DRILL_DB" \
                -v ON_ERROR_STOP=1 \
                -c "UPDATE audit_governance_outbox SET status = 3, last_error = '403 provisioning refusal (drill)' WHERE event_id = (SELECT event_id FROM audit_governance_outbox WHERE status = 0 LIMIT 1);"
            if C_OUT="$(DATABASE_URL="$T11_DRILL_URL" \
                    AERO__DATABASE__URL="$T11_DRILL_URL" \
                    cargo run -p aero-cli -- audit-provision-check 2>&1)"; then
                echo "✗ audit-provision-check leg C: expected non-zero exit with a dead row (fail-closed)" >&2
                echo "$C_OUT" >&2
                exit 1
            fi
            if ! grep -q "dead=1" <<<"$C_OUT" \
                || ! grep -q "delivered=0" <<<"$C_OUT" \
                || ! grep -q "audit-provision-check: dead:" <<<"$C_OUT" \
                || ! grep -q "verdict: fail-closed" <<<"$C_OUT"; then
                echo "✗ audit-provision-check leg C: expected dead=1 + delivered=0 + dead detail + fail-closed" >&2
                echo "$C_OUT" >&2
                exit 1
            fi
            echo "✓ audit-provision-check leg C: dead reported, never folded into delivered"
            b5_check "audit-provision-check" "PASS"
        fi
        # R5/A6.4: exercise the read-only sampler against this already-migrated
        # T-11 throwaway database before dropping it.  The test self-isolates
        # with TRUNCATE, so it cannot inherit the drill's terminal row or
        # mutate any provisioning verdict above.
        echo "▶ Running audit-outbox gauge cross-checks on the T-11 database..."
        if ! DATABASE_URL="$T11_DRILL_URL" \
            AERO__DATABASE__URL="$T11_DRILL_URL" \
            cargo test -p aero-server --test audit_outbox_gauges --locked \
                -- --ignored --test-threads=1; then
            echo "✗ audit-outbox gauge cross-checks failed" >&2
            exit 1
        fi
        echo "✓ audit-outbox gauge cross-checks passed"
        # Reuse the existing audit-provision-check contract slot; this is a
        # second evidence line for that slot, not a new B5 contract entry.
        b5_check "audit-provision-check" "PASS"
        drop_created_database "$T11_DRILL_DB"
        b5_check "t11-fail-closed" "PASS"
    else
        b5_check "t11-fail-closed" "SKIP (0239 not landed)"
        b5_check "audit-provision-check" "SKIP (0239 not landed)"
    fi
    # Moderation-priority drill (B5 acceptance, AC3): 500 backlog rows + 1
    # moderation row; the moderation row must be claimed and delivered first
    # (B5-3 claim order by priority DESC, higher = more urgent). The drill
    # FAILS red if 0239 lands without the priority ordering — the honest G6
    # signal that B5-3 has not landed (design F2/P4; the drill exits 2 SKIP
    # only when the 0239 table or its priority/class columns are absent).
    if [ -f "migrations/0239_audit_governance_outbox.sql" ]; then
        echo "▶ Creating fresh database for moderation-priority drill: ${PRIORITY_DRILL_DB}"
        create_throwaway_database "$PRIORITY_DRILL_DB"
        PRIORITY_DRILL_URL="${BASE_URL}/${PRIORITY_DRILL_DB}"
        echo "▶ Migrating database for moderation-priority drill..."
        DATABASE_URL="$PRIORITY_DRILL_URL" \
            AERO__DATABASE__URL="$PRIORITY_DRILL_URL" \
            cargo run --bin aero-cli -- migrate 2>&1 | tail -1
        # D8′ destructive-gate negative checks (design: priority-drill-
        # destructive-gate; INSIDE the 0239 gate — an un-migrated repo must
        # SKIP, never error on a missing table). Fixture: ONE delivered
        # (status=2) row — the ONLY bucket that reaches the gate without
        # tripping base fail-closed first (status 0/1 → base verdict; status
        # 3 → base verdict). Payload built with jsonb_build_object (no JSON
        # quoting inside the double-quoted -c string); event_id is a
        # mandatory NOT NULL PK with no default.
        echo "▶ Running priority-drill destructive-gate negative checks..."
        NEGATIVE_DRILL_LOG="$(mktemp "${TMPDIR:-/tmp}/aero-priority-drill-negative.XXXXXX")"
        FIXTURE_ID="$(run_psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d "$PRIORITY_DRILL_DB" \
            -v ON_ERROR_STOP=1 -q -tA \
            -c "INSERT INTO audit_governance_outbox (event_id, status, class, priority, payload) VALUES (gen_random_uuid(), 2, 'admin', 100, jsonb_build_object('event_id', gen_random_uuid()::text, 'source_system', 'aero-im.source', 'action', 'admin.content.flag')) RETURNING event_id;")"
        # (1) wrapper, no opt-in (hermetic =0 even if the developer shell
        #     exports 1) → REFUSED exit 1, row survives.
        set +e
        AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=0 \
            DATABASE_URL="$PRIORITY_DRILL_URL" \
            AERO__DATABASE__URL="$PRIORITY_DRILL_URL" \
            cargo run --quiet -p aero-cli -- audit-provision-check --priority \
            >"$NEGATIVE_DRILL_LOG" 2>&1
        NEG_RC=$?
        set -e
        if [ "$NEG_RC" -ne 1 ] || ! grep -q "REFUSED" "$NEGATIVE_DRILL_LOG"; then
            echo "✗ priority-drill guard: expected RC==1 + REFUSED (wrapper, no opt-in), got RC=$NEG_RC" >&2
            cat "$NEGATIVE_DRILL_LOG" >&2
            exit 1
        fi
        NEG_COUNT="$(run_psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d "$PRIORITY_DRILL_DB" \
            -v ON_ERROR_STOP=1 -tA -c "SELECT COUNT(*)::bigint FROM audit_governance_outbox;")"
        if [ "$NEG_COUNT" != "1" ]; then
            echo "✗ priority-drill guard: TRUNCATE ran — outbox count is $NEG_COUNT, expected 1" >&2
            exit 1
        fi
        echo "✓ priority-drill guard: REFUSED on non-empty outbox (wrapper, exit 1, row survived)"
        # (2) direct drill invocation, no opt-in → the in-tx gate refuses too
        #     (the drill's own usage header documents direct invocation; this
        #     also pre-warms the connector bin for the real drill below).
        set +e
        AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=0 \
            DATABASE_URL="$PRIORITY_DRILL_URL" \
            cargo run --quiet --locked -p aero-audit-connector --bin aero-audit-priority-drill \
            >"$NEGATIVE_DRILL_LOG" 2>&1
        NEG_RC=$?
        set -e
        if [ "$NEG_RC" -ne 1 ] || ! grep -q "REFUSED" "$NEGATIVE_DRILL_LOG"; then
            echo "✗ priority-drill guard: expected RC==1 + REFUSED (direct drill), got RC=$NEG_RC" >&2
            cat "$NEGATIVE_DRILL_LOG" >&2
            exit 1
        fi
        NEG_COUNT="$(run_psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d "$PRIORITY_DRILL_DB" \
            -v ON_ERROR_STOP=1 -tA -c "SELECT COUNT(*)::bigint FROM audit_governance_outbox;")"
        if [ "$NEG_COUNT" != "1" ]; then
            echo "✗ priority-drill guard: TRUNCATE ran on direct invocation — outbox count is $NEG_COUNT, expected 1" >&2
            exit 1
        fi
        echo "✓ priority-drill guard: REFUSED on direct drill invocation (in-tx gate, exit 1, row survived)"
        # (3) opt-in value typo (true ≠ 1) → still REFUSED (fail-closed env).
        set +e
        AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=true \
            DATABASE_URL="$PRIORITY_DRILL_URL" \
            AERO__DATABASE__URL="$PRIORITY_DRILL_URL" \
            cargo run --quiet -p aero-cli -- audit-provision-check --priority \
            >"$NEGATIVE_DRILL_LOG" 2>&1
        NEG_RC=$?
        set -e
        if [ "$NEG_RC" -ne 1 ] || ! grep -q "REFUSED" "$NEGATIVE_DRILL_LOG"; then
            echo "✗ priority-drill guard: expected RC==1 + REFUSED for 'true' (only '1' allows), got RC=$NEG_RC" >&2
            cat "$NEGATIVE_DRILL_LOG" >&2
            exit 1
        fi
        echo "✓ priority-drill guard: REFUSED on AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=true (only '1' allows)"
        # (4) DELETE the fixture (mandatory: drill round-1 COUNT(status=2)==100
        #     would break on a leftover delivered row; TRUNCATE-at-start is
        #     only the backstop). event_id-scoped.
        run_psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d "$PRIORITY_DRILL_DB" \
            -v ON_ERROR_STOP=1 \
            -c "DELETE FROM audit_governance_outbox WHERE event_id = '$FIXTURE_ID';" \
            >/dev/null
        rm -f "$NEGATIVE_DRILL_LOG"
        echo "▶ Running moderation-priority drill via aero-eng CLI (500 backlog + 1 moderation)..."
        PRIORITY_DRILL_LOG="$(mktemp "${TMPDIR:-/tmp}/aero-priority-drill.XXXXXX")"
        set +e
        AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1 \
            DATABASE_URL="$PRIORITY_DRILL_URL" \
            AERO__DATABASE__URL="$PRIORITY_DRILL_URL" \
            cargo run --quiet -p aero-cli -- audit-provision-check --priority \
            >"$PRIORITY_DRILL_LOG" 2>&1
        PRIORITY_DRILL_RC=$?
        set -e
        if [ "$PRIORITY_DRILL_RC" -eq 0 ]; then
            grep -q "priority: landed" "$PRIORITY_DRILL_LOG" || {
                echo "✗ priority drill: missing 'priority: landed' verdict line" >&2
                cat "$PRIORITY_DRILL_LOG" >&2
                exit 1
            }
            grep -q "drill: moderation-action-vocabulary: PASS" "$PRIORITY_DRILL_LOG" || {
                echo "✗ priority drill: missing 'drill: moderation-action-vocabulary: PASS'" >&2
                cat "$PRIORITY_DRILL_LOG" >&2
                exit 1
            }
            # B5-3 D-CAP starvation leg (additive): the drill's phase-2
            # 700-row drain must be observable in the gate.
            grep -q "drill: starvation-drain-700: PASS" "$PRIORITY_DRILL_LOG" || {
                echo "✗ priority drill: missing 'drill: starvation-drain-700: PASS'" >&2
                cat "$PRIORITY_DRILL_LOG" >&2
                exit 1
            }
            cat "$PRIORITY_DRILL_LOG"   # drill 的 PASS 行保持可见
            b5_check "moderation-priority-drill" "PASS"
            # The outbound-token pin rides the same drill: the seed-time
            # EXACT-equality bail + the `moderation-action-vocabulary` read-
            # back are the executable half of the coordinated-flip contract
            # (truth-check rule 3g covers the SQL literals statically).
            b5_check "moderation-outbound-token-exact" "PASS"
        elif [ "$PRIORITY_DRILL_RC" -eq 2 ]; then
            cat "$PRIORITY_DRILL_LOG"
            b5_check "moderation-priority-drill" "SKIP (priority/class not landed)"
            b5_check "moderation-outbound-token-exact" "SKIP (priority/class not landed)"
        else
            echo "✗ moderation-priority drill FAILED (exit $PRIORITY_DRILL_RC)" >&2
            cat "$PRIORITY_DRILL_LOG" >&2
            exit 1
        fi
        rm -f "$PRIORITY_DRILL_LOG"
        drop_created_database "$PRIORITY_DRILL_DB"
        # (Pre-existing duplicate PASS removed: the RC==0 branch above already
        # emitted PASS, and an unconditional PASS here contradicted the RC==2
        # SKIP path — SKIP-with-reason must be the terminal verdict.)
    else
        b5_check "moderation-priority-drill" "SKIP (0239 not landed)"
        b5_check "moderation-outbound-token-exact" "SKIP (0239 not landed)"
    fi
    # L1 window aggregation (B5-1 core, migration 0242): AC2 db_tests
    # (l1_window_aggregates_5_rows_to_1_outbox + concurrent merge) on their
    # own throwaway DB + the self-seeding parity drill (AC4 arbiter — the
    # non-vacuous trigger-presence detector). Gated on the 0242 migration
    # file (0239 precedent): absent → explicit SKIP verdicts, never silent
    # green. The gate also carries the cross-slice "exactly one 0242
    # definition" static arbiter: migration count == 242 and the
    # aero_enqueue_l1_aggregate_audit symbol appears in exactly the one
    # 0242 file (a second slice landing a renumbered copy reds here).
    if [ -f "migrations/0242_audit_governance_l1_aggregate.sql" ]; then
        MIGRATION_COUNT="$(ls migrations/*.sql | wc -l)"
        # Static arbiter counts ACTUAL files: 248 = 243 landed + 0246
        # (message-recall lane) + 0243 (login_failures_created_at_idx) + 0244
        # (audit_governance_failed_pairs DLQ) + 0247 (failed-pairs replay caps,
        # F-4).
        if [ "$MIGRATION_COUNT" -ne 248 ]; then
            echo "✗ 0242 static arbiter: expected exactly 248 migrations, found ${MIGRATION_COUNT}" >&2
            exit 1
        fi
        L1_DEFINITIONS="$(rg -l "aero_enqueue_l1_aggregate_audit" migrations/ 2>/dev/null || true)"
        if [ "$(printf '%s\n' "$L1_DEFINITIONS" | grep -c .)" -ne 1 ] \
            || ! grep -qx "migrations/0242_audit_governance_l1_aggregate.sql" <<<"$L1_DEFINITIONS"; then
            echo "✗ 0242 static arbiter: aero_enqueue_l1_aggregate_audit must be defined in exactly migrations/0242_audit_governance_l1_aggregate.sql" >&2
            echo "  found: $L1_DEFINITIONS" >&2
            exit 1
        fi
        echo "✓ 0242 static arbiter: 248 migrations, single aero_enqueue_l1_aggregate_audit definition"
        run_migrated_integration \
            "$L1_AGGREGATE_DB" \
            "l1_window_aggregates_" \
            "l1 window aggregate db_tests"
        echo "▶ Creating fresh database for L1 parity drill: ${L1_PARITY_DB}"
        create_throwaway_database "$L1_PARITY_DB"
        L1_PARITY_URL="${BASE_URL}/${L1_PARITY_DB}"
        echo "▶ Migrating database for L1 parity drill..."
        DATABASE_URL="$L1_PARITY_URL" \
            AERO__DATABASE__URL="$L1_PARITY_URL" \
            cargo run --bin aero-cli -- migrate 2>&1 | tail -1
        echo "▶ Running L1 parity drill (self-seeded through the 0242 trigger)..."
        DATABASE_URL="$L1_PARITY_URL" \
            cargo run --quiet --locked -p aero-audit-connector --bin aero-audit-l1-parity-drill
        drop_created_database "$L1_PARITY_DB"
        echo "✓ L1 parity drill passed (SUM(count) == COUNT(mapped audit), spill leg balanced)"
        b5_check "l1-aggregation-drill" "PASS"
    else
        echo "▶ Skipping L1 aggregation entries: migrations/0242_audit_governance_l1_aggregate.sql (B5-1 L1 slice) has not landed"
        b5_check "l1_window_aggregates_" "SKIP (0242 not landed)"
        b5_check "l1-aggregation-drill" "SKIP (0242 not landed)"
    fi
    # Room-lane enqueue (B5-1 completion, migration 0245): the two new
    # executed slots (room_lane_outbox_parity + message_lane_outbox_parity)
    # each own a throwaway DB (0239/0242 precedent). Gated on the 0245
    # migration file: absent → explicit SKIP verdicts, never silent green.
    # The gate also carries the cross-slice "exactly one 0245 definition"
    # static arbiter: the aero_enqueue_room_audit symbol must appear in
    # exactly migrations/0245_room_audit_governance.sql (a second slice
    # landing a renumbered copy reds here), and the single-definition
    # check cannot collide — the symbol exists in zero migrations today.
    if [ -f "migrations/0245_room_audit_governance.sql" ]; then
        ROOM_LANE_DEFINITIONS="$(rg -l "aero_enqueue_room_audit" migrations/ 2>/dev/null || true)"
        if [ "$(printf '%s\n' "$ROOM_LANE_DEFINITIONS" | grep -c .)" -ne 1 ] \
            || ! grep -qx "migrations/0245_room_audit_governance.sql" <<<"$ROOM_LANE_DEFINITIONS"; then
            echo "✗ 0245 static arbiter: aero_enqueue_room_audit must be defined in exactly migrations/0245_room_audit_governance.sql" >&2
            echo "  found: $ROOM_LANE_DEFINITIONS" >&2
            exit 1
        fi
        echo "✓ 0245 static arbiter: single aero_enqueue_room_audit definition in 0245"
        run_migrated_integration \
            "$ROOM_LANE_DB" \
            "room_lane_outbox_parity" \
            "room lane outbox parity db_tests"
        run_migrated_integration \
            "$MESSAGE_LANE_DB" \
            "message_lane_outbox_parity" \
            "message lane outbox parity db_tests"
    else
        echo "▶ Skipping room-lane entries: migrations/0245_room_audit_governance.sql (B5-1 room lane) has not landed"
        b5_check "room_lane_outbox_parity" "SKIP (0245 not landed)"
        b5_check "message_lane_outbox_parity" "SKIP (0245 not landed)"
    fi
    # Facade-level L1 window drill (B5-1, FR-1…FR-4, FR-6, FR-7): ImService
    # send/edit → in-tx audit → 0242 window → claim_due → real AuditRelay →
    # StubSink, on its own throwaway DB. Gated on 0239 (table) AND 0242
    # (trigger): absent → explicit SKIP, never silent green. The empty-filter
    # guard inside run_migrated_integration fails the slot if drill_facade_
    # matches zero tests (no vacuous green).
    if [ -f "migrations/0239_audit_governance_outbox.sql" ] \
        && [ -f "migrations/0242_audit_governance_l1_aggregate.sql" ]; then
        run_migrated_integration \
            "$FACADE_L1_WINDOW_DB" \
            "drill_facade_" \
            "facade L1 window drill db_tests" \
            "aero-im-core" \
            "facade-l1-window-drill"
    else
        b5_check "facade-l1-window-drill" "SKIP (0242 not landed)"
    fi
    # Room/recall-facade drills + R4 sink leg + connector-posture drill
    # (close-room direction, design §D9/DR-3): four named slots, each on its
    # own throwaway DB via run_migrated_integration (empty-filter guarded).
    # Gates: room lane 0245, recall lane 0246, connector posture + R4 sink
    # leg 0239. Absent migration → explicit SKIP, never silent green.
    if [ -f "migrations/0239_audit_governance_outbox.sql" ]; then
        if [ -f "migrations/0245_room_audit_governance.sql" ]; then
            run_migrated_integration \
                "$ROOM_FACADE_DRILL_DB" \
                "drill_room_" \
                "room lane facade drill db_tests" \
                "aero-im-core" \
                "room-lane-facade-drill"
        else
            b5_check "room-lane-facade-drill" "SKIP (0245 not landed)"
        fi
        if [ -f "migrations/0246_message_recall_audit_governance.sql" ]; then
            run_migrated_integration \
                "$RECALL_FACADE_DRILL_DB" \
                "drill_recall_" \
                "recall lane facade drill db_tests" \
                "aero-im-core" \
                "recall-lane-facade-drill"
        else
            b5_check "recall-lane-facade-drill" "SKIP (0246 not landed)"
        fi
        run_migrated_integration \
            "$POSTURE_DRILL_DB" \
            "drill_posture_" \
            "connector posture drill db_tests" \
            "aero-im-core" \
            "connector-posture-drill"
        run_migrated_integration \
            "$R4_SINK_LEG_DB" \
            "drill_payload_contract_" \
            "r4 sink leg drill db_tests" \
            "aero-im-core" \
            "drill-payload-contract-slot"
    else
        b5_check "room-lane-facade-drill" "SKIP (0239 not landed)"
        b5_check "recall-lane-facade-drill" "SKIP (0239 not landed)"
        b5_check "connector-posture-drill" "SKIP (0239 not landed)"
        b5_check "drill-payload-contract-slot" "SKIP (0239 not landed)"
    fi
    # B5-4 audit-outbox sampler smoke (design D5 A1): boot the real server
    # against a throwaway DB with backdated governance rows, poll /metrics
    # for the sampler gauges, exercise a slow tick + a DROP TABLE failure
    # leg, assert zero verdict-pollution lines. Gated on 0239 (the outbox
    # table) — absent → explicit SKIP, never silent green.
    if [ -f "migrations/0239_audit_governance_outbox.sql" ]         && [ -f "migrations/0248_audit_relay_provisioning.sql" ]; then
        echo "▶ Creating fresh database for audit-outbox sampler smoke: ${AUDIT_OUTBOX_SAMPLER_DB}"
        create_throwaway_database "$AUDIT_OUTBOX_SAMPLER_DB"
        SAMPLER_URL="${BASE_URL}/${AUDIT_OUTBOX_SAMPLER_DB}"
        echo "▶ Migrating database for audit-outbox sampler smoke..."
        DATABASE_URL="$SAMPLER_URL"             AERO__DATABASE__URL="$SAMPLER_URL"             cargo run --bin aero-cli -- migrate 2>&1 | tail -1
        # Keep the sampler DB in the same provisioned commercial state as a
        # real deployment: the Q0 runtime switch is on and an enabled binding
        # is present.  The sampler remains read-only and no commercial delivery
        # row is seeded.  Its runtime config below points only at a closed
        # loopback port, so boot is deterministic and cannot reach an external
        # service.
        SAMPLER_BINDINGS_FILE="$(mktemp "${TMPDIR:-/tmp}/aero-sampler-bindings.XXXXXX.json")"
        cat >"$SAMPLER_BINDINGS_FILE" <<'JSON'
{
  "version": 2,
  "bindings": [
    {
      "workspace_id": "00000000000000000000000000",
      "tenant_id": "tenant-b5-sampler",
      "billing_client_id": "client-b5-sampler",
      "billing_client_secret_env": "AERO_SAMPLER_BILLING_SECRET",
      "audit_client_id": "audit-client-b5-sampler",
      "audit_client_secret_env": "AERO_SAMPLER_AUDIT_SECRET",
      "revision": 1,
      "enabled": true
    }
  ]
}
JSON
        # The source_system is the config module's deterministic
        # `aero-im.<base64url(sha256(tenant_id))>` derivation for this fixture.
        run_psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d "$AUDIT_OUTBOX_SAMPLER_DB" \
            -v ON_ERROR_STOP=1 >/dev/null <<'SQL'
UPDATE snaplink_commercial_runtime
   SET enabled = TRUE, updated_at = clock_timestamp()
 WHERE singleton;
INSERT INTO snaplink_commercial_bindings
       (workspace_id, tenant_id, client_id, audit_client_id, source_system,
        revision, enabled)
VALUES ('00000000-0000-0000-0000-000000000000'::uuid,
        'tenant-b5-sampler', 'client-b5-sampler', 'audit-client-b5-sampler',
        'aero-im.jyFvjQV57ggGo2ShU1kOJXAf6WzEkr7Az1TOdJohAiM', 1, TRUE);
-- Seed: one enqueued row backdated 600s + one delivered row.  The sampler
-- is read-only — the 0239 trigger is not needed.
INSERT INTO audit_governance_outbox (event_id, status, attempts, priority, payload, available_at, created_at)
VALUES (gen_random_uuid(), 0, 0, 10, '{"event_id":"seed-1","source_system":"aero-im.source","action":"message.moderated"}'::jsonb,
        clock_timestamp() - interval '600 seconds', clock_timestamp() - interval '600 seconds');
INSERT INTO audit_governance_outbox (event_id, status, attempts, priority, payload, delivered_at, created_at)
VALUES (gen_random_uuid(), 2, 1, 10, '{"event_id":"seed-2","source_system":"aero-im.source","action":"message.moderated"}'::jsonb,
        clock_timestamp(), clock_timestamp() - interval '600 seconds');
SQL
        SAMPLER_LOG="$(mktemp "${TMPDIR:-/tmp}/aero-sampler.XXXXXX.log")"
        # Boot the real server (dev config.toml present; AERO__ envs win).
        # Commercial integration is enabled only against the deterministic
        # closed loopback endpoints above; no real external delivery is enabled.
        DATABASE_URL="$SAMPLER_URL" \
            AERO__DATABASE__URL="$SAMPLER_URL" \
            AERO__REDIS__URL="${REDIS_URL:-redis://localhost:6379}" \
            AERO__NATS__URL="${AERO__NATS__URL:-nats://localhost:4222}" \
            AERO__SERVER__PORT="${AERO_SAMPLER_PORT:-3030}" \
            AERO_METRICS_TOKEN=smoke-token \
            AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS=60 \
            AERO__SERVER__BLOB_DIR=/tmp/aero-sampler-blobs \
            AERO__SERVER__HLS_DIR=/tmp/aero-sampler-hls \
            AERO_SNAPLINK_COMMERCIAL_ENABLED=1 \
            AERO_SNAPLINK_ALLOW_INSECURE_LOOPBACK=1 \
            AERO_SNAPLINK_TOKEN_ENDPOINT=http://127.0.0.1:1/token \
            AERO_SNAPLINK_BILLING_BASE_URL=http://127.0.0.1:1 \
            AERO_SNAPLINK_AUDIT_BASE_URL=http://127.0.0.1:1 \
            AERO_SNAPLINK_BINDINGS_FILE="$SAMPLER_BINDINGS_FILE" \
            AERO_SNAPLINK_SOURCE_PREFIX=aero-im \
            AERO_SNAPLINK_LOG_PSEUDONYM_KEY=sampler-log-key-0123456789abcdef0123456789 \
            AERO_SAMPLER_BILLING_SECRET=sampler-billing-secret-0123456789abcdef \
            AERO_SAMPLER_AUDIT_SECRET=sampler-audit-secret-0123456789abcdef \
            AERO_SNAPLINK_REQUEST_TIMEOUT_SECS=1 \
            AERO_SNAPLINK_PROJECTION_INTERVAL_SECS=3600 \
            AERO_SNAPLINK_DELIVERY_POLL_MS=60000 \
            nohup cargo run --quiet --bin aero-server >"$SAMPLER_LOG" 2>&1 &
        SERVER_PID=$!
        sampler_ok=1
        # L1: poll /metrics (≤120s) for the sampler gauges.
        found=0
        for _ in $(seq 1 24); do
            sleep 5
            METRICS="$(curl -sf -H "Authorization: Bearer smoke-token"                 "http://localhost:${AERO_SAMPLER_PORT:-3030}/metrics" 2>/dev/null || true)"
            # Keep the probe pipe-free.  This script runs with `pipefail`, and
            # piping the ~60KiB metrics document into `grep -q` lets grep exit
            # early; the producer then receives SIGPIPE and makes a true match
            # look like a failed condition.
            if grep -q 'aero_audit_outbox_status{status="enqueued"} 1' <<<"$METRICS"                 && grep -q 'aero_audit_outbox_status{status="claimed"} 0' <<<"$METRICS"                 && grep -q 'aero_audit_outbox_status{status="delivered"} 1' <<<"$METRICS"                 && grep -q 'aero_audit_outbox_status{status="dead"} 0' <<<"$METRICS"                 && grep -q 'aero_audit_outbox_dead_rows 0' <<<"$METRICS"                 && grep -q 'aero_audit_outbox_sampler_up 1' <<<"$METRICS"; then
                found=1
                break
            fi
        done
        if [ "$found" -ne 1 ]; then
            echo "✗ audit-outbox-sampler L1: sampler gauges not observed" >&2
            tail -20 "$SAMPLER_LOG" >&2
            sampler_ok=0
        fi
        # L1 (AC1-C): the backdated status-0 row must surface as
        # oldest_pending_secs ≥ 600 (the seeded age — Q4 mirror parity).
        OLDEST1="$(awk '/^aero_audit_outbox_oldest_pending_secs / { print $2; exit }' <<<"$METRICS" || true)"
        if [ -z "${OLDEST1:-}" ] || [ "$OLDEST1" -lt 600 ]; then
            echo "✗ audit-outbox-sampler L1: oldest_pending_secs missing or < 600 (${OLDEST1:-absent})" >&2
            sampler_ok=0
        fi
        if grep -q "verdict:" "$SAMPLER_LOG"; then
            echo "✗ audit-outbox-sampler L1: verdict-pollution line in server log" >&2
            sampler_ok=0
        fi
        # L2: one slow tick (65s) — oldest strictly increases from L1,
        # delivered remains unchanged, and the Tier-1 sampler stays healthy.
        DELIVERED1="$(awk '/^aero_audit_outbox_status\{status="delivered"\} / { print $2; exit }' <<<"$METRICS" || true)"
        sleep 65
        METRICS2="$(curl -sf -H "Authorization: Bearer smoke-token"             "http://localhost:${AERO_SAMPLER_PORT:-3030}/metrics" 2>/dev/null || true)"
        OLDEST2="$(awk '/^aero_audit_outbox_oldest_pending_secs / { print $2; exit }' <<<"$METRICS2" || true)"
        DELIVERED2="$(awk '/^aero_audit_outbox_status\{status="delivered"\} / { print $2; exit }' <<<"$METRICS2" || true)"
        if [ -z "${OLDEST1:-}" ] || [ -z "${OLDEST2:-}" ] || [ "$OLDEST2" -le "$OLDEST1" ]; then
            echo "✗ audit-outbox-sampler L2: oldest_pending_secs did not strictly increase from L1 (${OLDEST1:-absent} -> ${OLDEST2:-absent})" >&2
            sampler_ok=0
        fi
        if [ -z "${DELIVERED1:-}" ] || [ -z "${DELIVERED2:-}" ] || [ "$DELIVERED2" != "$DELIVERED1" ]; then
            echo "✗ audit-outbox-sampler L2: delivered bucket changed (${DELIVERED1:-absent} -> ${DELIVERED2:-absent})" >&2
            sampler_ok=0
        fi
        if ! grep -q 'aero_audit_outbox_sampler_up 1' <<<"$METRICS2"; then
            echo "✗ audit-outbox-sampler L2: sampler_up was not 1 after the slow tick" >&2
            sampler_ok=0
        fi
        # L3: DROP TABLE → sampler_up 0, errors ≥1, enqueued retains last value.
                run_psql -h "$PSQL_HOST" -p "$PSQL_PORT" -U "$PSQL_USER" -d "$AUDIT_OUTBOX_SAMPLER_DB" \
            -c "DROP TABLE audit_governance_outbox;" >/dev/null
        # Poll (≤45s) — poll immediately and then every 5s.  Nine samples
        # cover a full 30s Tier-1 phase with 15s slack while retaining the
        # design's bounded failure window.
        l3_ok=0
        METRICS3=""
        for attempt in $(seq 1 9); do
            METRICS3="$(curl -sf -H "Authorization: Bearer smoke-token"                 "http://localhost:${AERO_SAMPLER_PORT:-3030}/metrics" 2>/dev/null || true)"
            if grep -q 'aero_audit_outbox_sampler_up 0' <<<"$METRICS3"                 && grep -Eq 'aero_audit_outbox_sample_errors_total [1-9][0-9]*' <<<"$METRICS3"; then
                l3_ok=1
                break
            fi
            if [ "$attempt" -lt 9 ]; then
                sleep 5
            fi
        done
        if [ "$l3_ok" -ne 1 ]; then
            echo "✗ audit-outbox-sampler L3: fail-closed sampler state not observed" >&2
            echo "$METRICS3" | grep -E "sampler_up|sample_errors|enqueued" >&2
            sampler_ok=0
        fi
        # L3 (F2): the enqueued gauge retains its last value — never a
        # fabricated zero snapshot on a query failure.
        if ! grep -q 'aero_audit_outbox_status{status="enqueued"} 1' <<<"$METRICS3"; then
            echo "✗ audit-outbox-sampler L3: enqueued gauge did not retain its last value (fabricated zero snapshot)" >&2
            sampler_ok=0
        fi
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
        rm -f "$SAMPLER_LOG" "$SAMPLER_BINDINGS_FILE"
        SAMPLER_BINDINGS_FILE=""
        drop_created_database "$AUDIT_OUTBOX_SAMPLER_DB"
        if [ "$sampler_ok" -eq 1 ]; then
            echo "✓ audit-outbox-sampler smoke passed"
            b5_check "audit-outbox-sampler" "PASS"
        else
            echo "✗ audit-outbox-sampler smoke FAILED" >&2
            exit 1
        fi
    else
        b5_check "audit-outbox-sampler" "SKIP (0239/0248 not landed)"
    fi
    # Notification fan-out suite (AT-1…AT-7): owns a fresh throwaway DB with
    # its own migration + required Redis presence leg; the shared main-DB run
    # below must --skip db_tests:: (its relay loops claim global state). This
    # is the always-run relay-coverage leg (R6/AC4).
    echo "▶ Running notification fan-out integration suite (fresh DB + Redis presence leg)..."
    bash scripts/test-notification-fanout.sh
    b5_check "notification-fanout" "PASS"
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
# are always excluded here because this database is deliberately non-empty;
# the aero-im-core `db_tests::notifications_tests`/`relay_tests` and B5
# `audit_governance::` + `db_tests::governance_drill_tests` +
# `db_tests::moderation_finalize_drill_tests` suites are
# excluded because each runs on its OWN fresh throwaway DB (the
# notification fan-out step above runs the whole aero-im-core `db_tests::`
# module; the B5-1 named slots run `audit_governance::` and the aero-ai
# moderation-finalize drill). The last three are
# additionally load-bearing on the shared main DB: all flip the GLOBAL
# `snaplink_commercial_runtime.enabled` singleton (aero-im-core drills hold
# it ON through 1.2 s sleeps) and TRUNCATE `audit_governance_outbox` — with
# the aero-storage and aero-im-core binaries running CONCURRENTLY on the one
# shared DB, the drills' ON window makes the other binary's unbound message
# INSERTs raise P0001 (0235 metering; empirically `message_reports::db_tests`
# 3/3) and the drills' TRUNCATE wipes the other binary's just-committed
# governance row mid-assertion. Own-DB execution keeps both covered with
# zero cross-binary leakage.
# `--jobs 1` additionally serializes the test BINARIES themselves: the
# aero-audit-connector `pg.rs` db_tests (concurrent_double_claim_…,
# expired_lease_…, mixed_priority_claim_…) self-isolate by TRUNCATing the
# SAME outbox on the shared DB (they have no own-DB slot — the A3/T-11
# slots cover only the bin drills), and a concurrent binary's TRUNCATE can
# land between another binary's commit and its governance-row assertion.
# Serial binaries give every self-isolating TRUNCATE a strictly-before
# ordering — the TRUNCATE-race has no window left.
echo "▶ Running integration tests..."
echo ""
set +e
cargo test --workspace --lib --locked --jobs 1 -- \
    --ignored --test-threads=1 \
    --skip eicar_is_reported_infected_against_real_clamd \
    --skip rolling_upgrade_fences_are_atomic_before_0176_reasserts_them \
    --skip migration_0192_repairs_attempted_cross_room_scheduled_replies \
    --skip migration_0228_backfills_workspace_bot_membership \
    --skip migration_0233_backfills_and_constrains_human_identity_issuer \
    --skip migration_0237_backfills_before_installing_destination_guard \
    --skip message_quota_and_snaplink_outboxes_are_transactional \
    --skip scim_inactive_first_nil_workspace_member_rolls_back_owner_bootstrap \
    --skip audit_governance \
    --skip db_tests::governance_drill_tests \
    --skip db_tests::moderation_finalize_drill_tests \
    --skip db_tests::notifications_tests \
    --skip db_tests::relay_tests
TEST_EXIT=$?
set -e

echo ""
if [ "$TEST_EXIT" -eq 0 ]; then
    echo "✓ All integration tests passed"
else
    echo "✗ Some integration tests failed (exit code: $TEST_EXIT)"
    exit "$TEST_EXIT"
fi

# ---- B5 acceptance gate closure (relay coverage + contract pin) ----
# Relay-mock probe leg (sibling B5-2): DB-free black-box probe suite over the
# connector state machine (mock sink instead of a global-state relay loop);
# gated on the probe bin being landed — explicit SKIP verdict otherwise.
if [ -f "crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs" ]; then
    if PROBE_OUT="$(cargo run --quiet -p aero-cli -- network relay-probe 2>&1)"; then
        if [ "$(grep -c '^probe: .*: PASS$' <<<"$PROBE_OUT")" -eq 9 ]; then
            # The optional mock URL is a real reachability preflight.  Keep a
            # fixed closed loopback port here so a future regression that
            # silently ignores the argument cannot turn this leg green.
            if NEGATIVE_PROBE_OUT="$(cargo run --quiet -p aero-cli -- network relay-probe http://127.0.0.1:1 2>&1)"; then
                echo "✗ B5 relay-mock probe: unreachable mock URL unexpectedly succeeded" >&2
                echo "$NEGATIVE_PROBE_OUT" >&2
                b5_check "relay-mock-probe" "FAIL (mock-url preflight)"
                exit 1
            elif ! grep -q "relay mock unreachable" <<<"$NEGATIVE_PROBE_OUT"; then
                echo "✗ B5 relay-mock probe: unreachable mock URL did not fail closed" >&2
                echo "$NEGATIVE_PROBE_OUT" >&2
                b5_check "relay-mock-probe" "FAIL (mock-url preflight message)"
                exit 1
            fi
            b5_check "relay-mock-probe" "PASS"
        else
            echo "✗ B5 relay-mock probe: expected 9 'probe: <name>: PASS' scenario lines" >&2
            echo "$PROBE_OUT" >&2
            b5_check "relay-mock-probe" "FAIL (probe verdict count)"
            exit 1
        fi
    else
        echo "✗ B5 relay-mock probe FAILED" >&2
        echo "$PROBE_OUT" >&2
        b5_check "relay-mock-probe" "FAIL"
        exit 1
    fi
else
    b5_check "relay-mock-probe" "SKIP (B5-2 relay probe not landed)"
fi

# Relay coverage (AC4): at least one relay-execution leg must have actually
# run per invocation (fan-out suite / A3 drill / relay-mock probe) — the
# story is never a wholesale skip. Degraded under SKIP_DB_CREATE (the fresh
# legs did not run by design; probe is DB-free and still counts).
relay_legs=0
grep -q "^B5-CHECK notification-fanout: PASS$" "$B5_LOG" && relay_legs=$((relay_legs + 1))
grep -q "^B5-CHECK a3-relay-drill: PASS$" "$B5_LOG" && relay_legs=$((relay_legs + 1))
grep -q "^B5-CHECK relay-mock-probe: PASS$" "$B5_LOG" && relay_legs=$((relay_legs + 1))
if [ -z "$SKIP_DB_CREATE" ] && [ "$relay_legs" -eq 0 ]; then
    echo "✗ B5 relay coverage: zero legs executed (wholesale skip)" >&2
    exit 1
fi
echo "B5 relay coverage: ${relay_legs} leg(s) executed (fan-out + A3 + relay-mock)"

# Contract pin (AC1): exactly 48 named slots, no dupes, no malformed
# entries, ≥1 executed slot, and every executed slot backed by a
# `B5-CHECK <name>: PASS|SKIP` verdict line (fresh mode).
if ! assert_b5_contract_pin "$B5_LOG"; then
    exit 1
fi

# Gauge pin (R6): the five public audit-outbox series are a separate shape
# contract, so adding them never changes the 48/48 test-slot count.  Keep a
# second source-side pin here so a bash spelling drift cannot silently leave a
# dashboard series un-emitted by aero-common.
if ! assert_b5_gauge_pin; then
    exit 1
fi
for gauge_name in "${B5_GAUGE_PIN[@]}"; do
    if ! grep -Fq "$gauge_name" "crates/aero-common/src/metrics.rs"; then
        echo "✗ B5 gauge source pin: '${gauge_name}' is absent from aero-common metric names" >&2
        exit 1
    fi
done
echo "B5 gauge source pin: 5/5: PASS"

# F4 stub-reachability pin (merged B5-2): production construction paths must
# never reference the test stub / its alg:none minting.
if ! assert_no_production_stub_references; then
    exit 1
fi
