#!/usr/bin/env bash
# Regression tests for the B5 48/48 contract pin guard (scripts/b5-pin.sh).
# Pure bash — no database, no services. Exercises the guard's positive case
# and every negative case the B5 acceptance requires:
#   count ≠ 48 → FAIL; duplicates → FAIL; malformed slot → FAIL;
#   vacuous list (all [PROPOSED]) → FAIL; missing verdict evidence → FAIL;
#   SKIP_DB_CREATE degradation → PASS with a degraded note.
# Exits non-zero when any case regresses.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
source "$ROOT/scripts/b5-pin.sh"
ORIG_LIST=("${B5_CONTRACT_TEST_LIST[@]}")

failures=0
TMP_LOGS=()

check() { # check <name> <expected-grep-pattern> <actual-output>
    local name="$1" pattern="$2" actual="$3"
    if grep -q "$pattern" <<<"$actual"; then
        echo "ok: $name"
    else
        echo "FAIL: $name (expected output matching '$pattern', got: $actual)" >&2
        failures=$((failures + 1))
    fi
}

temp_log() {
    B5_LOG="$(mktemp "${TMPDIR:-/tmp}/aero-b5-pin-log.XXXXXX")"
    TMP_LOGS+=("$B5_LOG")
}

cleanup() {
    rm -f "${TMP_LOGS[@]}"
}
trap cleanup EXIT INT TERM

# Fabricate full verdict evidence for the 27 executed slots (all PASS).
positive_log() {
    : >"$B5_LOG"
    for entry in "${ORIG_LIST[@]}"; do
        [[ "$entry" == *"[PROPOSED]" ]] && continue
        echo "B5-CHECK ${entry}: PASS" >>"$B5_LOG"
    done
}

# run_guard <name> <expected-pattern> — runs the guard (stdout+stderr
# captured) against the CURRENT B5_CONTRACT_TEST_LIST and checks the pattern.
run_guard() {
    local name="$1" pattern="$2"
    local out
    out="$(assert_b5_contract_pin "$B5_LOG" 2>&1)" || true
    check "$name" "$pattern" "$out"
}

restore_list() {
    B5_CONTRACT_TEST_LIST=("${ORIG_LIST[@]}")
}

# --- list shape ---
temp_log
positive_log
run_guard "list-is-exactly-48-slots" "B5 contract pin: 48/48 (27 executed, 21 \[PROPOSED\]): PASS"
b5_check "t11-fail-closed" "PASS"
check "b5-check-appends-to-log" "B5-CHECK t11-fail-closed: PASS" "$(cat "$B5_LOG")"

# --- count ≠ 47 (36 slots) ---
# (A 36-slice of the 47-list prints `47/36` — the count check fires first.)
temp_log
positive_log
restore_list
B5_CONTRACT_TEST_LIST=("${ORIG_LIST[@]:0:36}")
run_guard "count-36-fails" "B5 contract pin: 48/36"

# --- duplicate slot (still 48 entries) ---
temp_log
positive_log
restore_list
B5_CONTRACT_TEST_LIST=("${ORIG_LIST[@]}")
B5_CONTRACT_TEST_LIST[36]="${B5_CONTRACT_TEST_LIST[0]}"
run_guard "duplicate-slot-fails" "duplicate slot"

# --- malformed slot ---
temp_log
positive_log
restore_list
B5_CONTRACT_TEST_LIST=("${ORIG_LIST[@]}")
B5_CONTRACT_TEST_LIST[36]="bad name!"
run_guard "malformed-slot-fails" "malformed slot"

# --- vacuous list (48 [PROPOSED] slots, zero executed) ---
temp_log
positive_log
restore_list
B5_CONTRACT_TEST_LIST=()
for i in $(seq -w 1 48); do
    B5_CONTRACT_TEST_LIST+=("contract-test-${i}[PROPOSED]")
done
run_guard "vacuous-list-fails" "vacuous list"

# --- missing verdict evidence for an executed slot ---
temp_log
positive_log
restore_list
grep -v "^B5-CHECK t11-fail-closed:" "$B5_LOG" >"${B5_LOG}.tmp" && mv "${B5_LOG}.tmp" "$B5_LOG"
run_guard "missing-verdict-fails" "no verdict line for executed slot 't11-fail-closed'"

# --- SKIP_DB_CREATE degradation: empty log still passes shape checks ---
temp_log
restore_list
out="$(SKIP_DB_CREATE=1 assert_b5_contract_pin "$B5_LOG" 2>&1)" || true
check "skip-db-create-degrades-with-note" "verdict evidence degraded" "$out"
check "skip-db-create-still-pins" "B5 contract pin: 48/48 (27 executed, 21 \[PROPOSED\]): PASS" "$out"

if [ "$failures" -ne 0 ]; then
    echo "✗ b5 pin guard: ${failures} regression case(s) failed" >&2
    exit 1
fi
echo "✓ b5 pin guard: all regression cases passed"
