#!/usr/bin/env bash
# Hermetic regression suite for the incremental rustfmt gate.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GUARD="$ROOT/scripts/fmt-check.sh"
TMP="$(mktemp -d -t aero-fmt-guard-test.XXXXXX)"
PASSED=0

cleanup() {
    rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

new_repo() {
    local name="$1"
    local style="${2:-formatted}"
    local repo="$TMP/$name"
    mkdir -p "$repo/crates/demo/src"
    if [[ "$style" == formatted ]]; then
        printf 'pub fn baseline() {}\n' >"$repo/crates/demo/src/lib.rs"
    else
        printf 'pub fn baseline(){ }\n' >"$repo/crates/demo/src/lib.rs"
    fi
    git -C "$repo" init -q
    git -C "$repo" config user.email fmt-guard@example.invalid
    git -C "$repo" config user.name fmt-guard
    git -C "$repo" add crates
    git -C "$repo" commit -qm baseline
    printf '%s\n' "$repo"
}

run_guard() {
    local repo="$1"
    AERO_FORMAT_ROOT="$repo" AERO_FORMAT_BASE_REF=HEAD "$GUARD"
}

pass() {
    PASSED=$((PASSED + 1))
    printf 'fmt-guard-test: %s: PASS\n' "$1"
}

expect_failure() {
    local label="$1"
    local repo="$2"
    local output="$repo/fmt.err"
    if run_guard "$repo" >"$output" 2>&1; then
        printf 'fmt-guard-test: %s unexpectedly passed\n' "$label" >&2
        exit 1
    fi
    if ! grep -Fq 'new rustfmt drift' "$output"; then
        printf 'fmt-guard-test: %s missed drift diagnostic\n' "$label" >&2
        sed -n '1,120p' "$output" >&2
        exit 1
    fi
    pass "$label"
}

repo="$(new_repo clean)"
run_guard "$repo" >/dev/null
pass clean_baseline

printf '\npub fn added() {}\n' >>"$repo/crates/demo/src/lib.rs"
printf 'pub fn second() {}\n' >"$repo/crates/demo/src/second.rs"
run_guard "$repo" >/dev/null
pass formatted_changes

repo="$(new_repo drift)"
printf '\npub fn drift(){ }\n' >>"$repo/crates/demo/src/lib.rs"
expect_failure formatted_baseline_drift "$repo"

repo="$(new_repo new_file)"
printf 'pub fn new_file(){ }\n' >"$repo/crates/demo/src/new.rs"
expect_failure unformatted_new_file "$repo"

repo="$(new_repo legacy unformatted)"
printf 'pub fn more_debt(){ }\n' >>"$repo/crates/demo/src/lib.rs"
legacy_output="$(run_guard "$repo")"
grep -Fq 'WARN legacy rustfmt debt' <<<"$legacy_output"
pass legacy_debt_warns_without_blocking

rustfmt --edition 2021 "$repo/crates/demo/src/lib.rs"
fixed_output="$(run_guard "$repo")"
if grep -Fq 'WARN legacy rustfmt debt' <<<"$fixed_output"; then
    echo 'fmt-guard-test: fixed legacy file still warned' >&2
    exit 1
fi
pass legacy_debt_can_be_fixed

printf 'fmt-guard-test: PASS (%d/6)\n' "$PASSED"
