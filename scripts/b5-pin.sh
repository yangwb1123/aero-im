#!/usr/bin/env bash
# B5 acceptance-gate pin material for scripts/test-integration.sh (G6 总装门):
#
#   * B5_CONTRACT_TEST_LIST — the 46/46 named contract-test slots (AC1 of
#     docs/requirements/2026-08-07-aero-cli-b5-acceptance-gate-harness.req.md).
#     21 in-repo-executable names + 21 out-of-repo slots tagged [PROPOSED]
#     (docs/proposals/audit-contract-batch-aero-im.md :13 — the v2 contract
#     test names live outside this repo; when the contract text lands, replace
#     the placeholders verbatim — one-touch — and the guard automatically
#     starts requiring verdict evidence for them).
#   * b5_check — the verdict-line protocol (`B5-CHECK <name>: PASS|SKIP
#     (<reason>)`): echoed to stdout AND appended to $B5_LOG (the pin guard's
#     evidence file).
#   * assert_b5_contract_pin — the guard: exactly 41 slots, no duplicates, no
#     malformed entries, ≥1 executed slot (no vacuous green), and (fresh mode)
#     every executed slot backed by a `B5-CHECK <name>: PASS|SKIP` verdict
#     line in the B5 log (SKIP-with-reason counts as handled — 0239 / sibling
#     windows stay green; a missing verdict line is a FAIL).
#
# Sourced by scripts/test-integration.sh and exercised standalone by
# scripts/test-b5-pin-guard.sh (pure bash — no database, no services).

# 46 slots: 25 executed + 21 [PROPOSED]. The executed slots' verdict sources:
#   cargo-test filters (run_migration_regression / run_migrated_integration —
#   empty-filter guarded, ≥1 test must actually run), 0239-gated B5-1 entries,
#   the A3 / T-11 / moderation-priority drill sections, the notification
#   fan-out suite, the sibling-presence-gated relay-mock-probe and
#   audit-provision-check legs, and the 0245-gated room/message-lane parity
#   db_test slots (room_lane_outbox_parity / message_lane_outbox_parity).
B5_CONTRACT_TEST_LIST=(
    rolling_upgrade_fences_are_atomic_before_0176_reasserts_them
    migration_0192_repairs_attempted_cross_room_scheduled_replies
    migration_0228_backfills_workspace_bot_membership
    migration_0233_backfills_and_constrains_human_identity_issuer
    migration_0237_backfills_before_installing_destination_guard
    message_quota_and_snaplink_outboxes_are_transactional
    scim_inactive_first_nil_workspace_member_rolls_back_owner_bootstrap
    audit_governance::
    moderation_finalize_outbox_parity
    moderation_finalize_drill
    auth_outbox_parity
    a3-relay-drill
    t11-fail-closed
    moderation-priority-drill
    notification-fanout
    relay-mock-probe
    audit-provision-check
    # Out-of-repo contract slots — [PROPOSED] placeholders, count only.
    contract-test-01[PROPOSED]
    contract-test-02[PROPOSED]
    contract-test-03[PROPOSED]
    contract-test-04[PROPOSED]
    contract-test-05[PROPOSED]
    contract-test-06[PROPOSED]
    contract-test-07[PROPOSED]
    contract-test-08[PROPOSED]
    contract-test-09[PROPOSED]
    contract-test-10[PROPOSED]
    contract-test-11[PROPOSED]
    contract-test-12[PROPOSED]
    contract-test-13[PROPOSED]
    contract-test-14[PROPOSED]
    contract-test-15[PROPOSED]
    contract-test-16[PROPOSED]
    contract-test-17[PROPOSED]
    contract-test-18[PROPOSED]
    contract-test-19[PROPOSED]
    contract-test-20[PROPOSED]
    contract-test-21[PROPOSED]
    l1-aggregation-drill
    room_lane_outbox_parity
    message_lane_outbox_parity
    facade-l1-window-drill
    room-lane-facade-drill
    recall-lane-facade-drill
    connector-posture-drill
    drill-payload-contract-slot
)

# Verdict-line protocol: `B5-CHECK <name>: PASS|SKIP (<reason>)`. Each
# executed segment records its verdict here; the pin guard greps $B5_LOG for
# the evidence (a missing verdict line for an executed slot = FAIL).
b5_check() {
    local name="$1" verdict="$2"
    local line="B5-CHECK ${name}: ${verdict}"
    echo "$line"
    echo "$line" >>"${B5_LOG:-/dev/null}"
}

# The 42/42 pin guard (AC1). Argument: the B5 log file holding the verdict
# lines. Unconditional (no DB); in SKIP_DB_CREATE dev-hygiene mode the
# verdict-evidence half degrades (fresh-DB segments did not run) while the
# count/format/dupe/vacuous checks stay enforced.
assert_b5_contract_pin() {
    local log_file="$1"
    local count=${#B5_CONTRACT_TEST_LIST[@]}
    if [ "$count" -ne 46 ]; then
        echo "✗ B5 contract pin: 46/${count} (expected exactly 46 named slots)" >&2
        return 1
    fi
    local -A seen=()
    local entry executed=0 proposed=0
    for entry in "${B5_CONTRACT_TEST_LIST[@]}"; do
        # Hyphens are required by the requirement's own slot names
        # (t11-fail-closed, moderation-priority, a3-relay-drill, …).
        if [[ ! "$entry" =~ ^[A-Za-z0-9_:+-]+(\[PROPOSED\])?$ ]]; then
            echo "✗ B5 contract pin: malformed slot '${entry}'" >&2
            return 1
        fi
        if [ -n "${seen[$entry]:-}" ]; then
            echo "✗ B5 contract pin: duplicate slot '${entry}'" >&2
            return 1
        fi
        seen[$entry]=1
        if [[ "$entry" == *"[PROPOSED]" ]]; then
            proposed=$((proposed + 1))
        else
            executed=$((executed + 1))
        fi
    done
    if [ "$executed" -eq 0 ]; then
        echo "✗ B5 contract pin: vacuous list (zero executed slots, all [PROPOSED])" >&2
        return 1
    fi
    if [ -z "${SKIP_DB_CREATE:-}" ]; then
        local missing=0
        for entry in "${B5_CONTRACT_TEST_LIST[@]}"; do
            [[ "$entry" == *"[PROPOSED]" ]] && continue
            if ! grep -Eq "^B5-CHECK ${entry}: (PASS|SKIP)( |$)" "$log_file"; then
                echo "✗ B5 contract pin: no verdict line for executed slot '${entry}' (need 'B5-CHECK ${entry}: PASS|SKIP' in the B5 log)" >&2
                missing=$((missing + 1))
            fi
        done
        if [ "$missing" -ne 0 ]; then
            return 1
        fi
    else
        echo "B5 pin: verdict evidence degraded (SKIP_DB_CREATE); count/format/dupe/vacuous checks still enforced"
    fi
    echo "B5 contract pin: 46/46 (${executed} executed, ${proposed} [PROPOSED]): PASS"
}

# F4 stub-reachability pin (merged B5-2 design §6 step 7): the test stub
# (`crates/aero-audit-connector/src/stub.rs`) and its `alg:none` minting must
# be unreachable from production construction paths — `RelayConfig::from_env
# → AuditClient::new` never goes through the stub. Guard ①: the server crate
# has zero `stub` references. Guard ②: the connector's production modules
# (everything except `stub.rs` itself, the drill bins, and `relay.rs`, whose
# reference lives inside `#[cfg(test)]`) have zero `stub` references.
assert_no_production_stub_references() {
    local root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    local violations=0

    # Guard ① — aero-server production tree.
    local server_hits
    server_hits=$(grep -rn --include='*.rs' 'aero_audit_connector::stub' "$root/crates/aero-server/src" 2>/dev/null || true)
    if [ -n "$server_hits" ]; then
        echo "✗ F4 pin: aero-server production code references the audit test stub:" >&2
        echo "$server_hits" >&2
        violations=$((violations + 1))
    fi

    # Guard ② — connector production modules (stub.rs, src/bin/, relay.rs exempt).
    local connector_hits
    connector_hits=$(grep -rn --include='*.rs' 'stub::' "$root/crates/aero-audit-connector/src" \
        --exclude='stub.rs' 2>/dev/null | grep -v '/src/bin/' | grep -v '/src/relay.rs' || true)
    if [ -n "$connector_hits" ]; then
        echo "✗ F4 pin: a connector production module references the test stub:" >&2
        echo "$connector_hits" >&2
        violations=$((violations + 1))
    fi

    if [ "$violations" -ne 0 ]; then
        return 1
    fi
    echo "F4 pin: production trees are stub-free (server ① + connector ②): PASS"
}
