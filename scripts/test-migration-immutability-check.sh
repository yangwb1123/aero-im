#!/usr/bin/env bash
# Hermetic regression suite for migration-immutability-check.sh. It creates
# only disposable Git repositories and needs no database or network access.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GUARD="$ROOT/scripts/migration-immutability-check.sh"
TMP="$(mktemp -d -t aero-migration-guard-test.XXXXXX)"
PASSED=0

cleanup() {
    rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

new_repo() {
    local name="$1"
    local sequence="${2:-0001}"
    local repo="$TMP/$name"
    mkdir -p "$repo/migrations"
    printf 'SELECT 1;\n' >"$repo/migrations/${sequence}_baseline.sql"
    git -C "$repo" init -q
    git -C "$repo" config user.email migration-guard@example.invalid
    git -C "$repo" config user.name migration-guard
    git -C "$repo" add migrations
    git -C "$repo" commit -qm baseline
    printf '%s\n' "$repo"
}

run_guard() {
    local repo="$1"
    AERO_MIGRATION_ROOT="$repo" \
        AERO_MIGRATION_BASE_REF=HEAD \
        "$GUARD"
}

pass() {
    PASSED=$((PASSED + 1))
    printf 'migration-guard-test: %s: PASS\n' "$1"
}

expect_failure() {
    local label="$1"
    local expected="$2"
    local repo="$3"
    local output="$repo/guard.err"

    if run_guard "$repo" >"$output" 2>&1; then
        printf 'migration-guard-test: %s unexpectedly passed\n' "$label" >&2
        exit 1
    fi
    if ! grep -Fq "$expected" "$output"; then
        printf 'migration-guard-test: %s missed diagnostic %q\n' "$label" "$expected" >&2
        sed -n '1,120p' "$output" >&2
        exit 1
    fi
    pass "$label"
}

repo="$(new_repo clean)"
run_guard "$repo" >/dev/null
pass clean_baseline

printf 'SELECT 2;\n' >"$repo/migrations/0002_added.sql"
run_guard "$repo" >/dev/null
pass higher_sequence_addition

repo="$(new_repo edit)"
printf '%s\n' '-- forbidden edit' >>"$repo/migrations/0001_baseline.sql"
expect_failure historical_edit 'historical migrations changed' "$repo"

repo="$(new_repo delete)"
printf 'SELECT 2;\n' >"$repo/migrations/0002_second.sql"
git -C "$repo" add migrations/0002_second.sql
git -C "$repo" commit -qm second
rm "$repo/migrations/0001_baseline.sql"
expect_failure historical_delete 'historical migrations changed' "$repo"

repo="$(new_repo duplicate)"
printf 'SELECT 2;\n' >"$repo/migrations/0001_duplicate.sql"
expect_failure duplicate_sequence 'duplicate sequence 0001' "$repo"

repo="$(new_repo nonmonotonic 0002)"
printf 'SELECT 1;\n' >"$repo/migrations/0001_late.sql"
expect_failure nonmonotonic_addition 'must follow baseline sequence 0002' "$repo"

repo="$(new_repo invalid)"
printf 'SELECT 2;\n' >"$repo/migrations/0002-Bad.sql"
expect_failure invalid_filename 'expected NNNN_lower_snake_case.sql' "$repo"

repo="$(new_repo rename)"
git -C "$repo" mv migrations/0001_baseline.sql migrations/0002_renamed.sql
expect_failure historical_rename 'historical migrations changed' "$repo"

printf 'migration-guard-test: PASS (%d/8)\n' "$PASSED"
