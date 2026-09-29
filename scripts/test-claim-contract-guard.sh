#!/usr/bin/env bash
# Regression tests for the claim-contract guard (scripts/truth-check-lib.sh,
# truth-check §3, AC4 pattern). Pure bash — no database, no services.
#
# Covers, per the §1.5 re-spec in
# docs/design/2026-08-08-aero-auth-client-credentials-claim-contract-leaf.design.md:
#   ① clean tree (the REAL tree — post-implementation) → 0 violations;
#   ② negative cases on a COPY of the real tree (appending literals to an
#      EXISTING non-exempt file — no orphan-confound: a new file under
#      crates/*/src would itself trip truth-check §1's orphan scan, so the
#      injections never create files);
#   ③ skip rules (any `tests` path component / comment lines / stub.rs /
#      leaf exemption) and allowlist file:line:literal granularity;
#   ④ case-insensitive at+jwt typ matching; exp/nbf exclusion (F6);
#   ⑤ usage-side scan (logic-duplication hole);
#   ⑥ audit-flag mirror (AC4);
#   ⑦ stale-allowlist warning (drift → re-pin);
#   ⑧ exit-code fold into truth-check (F5) via a wired copy.
# Exits non-zero when any case regresses.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
source "$ROOT/scripts/truth-check-lib.sh"

failures=0
TMP="$(mktemp -d "${TMPDIR:-/tmp}/aero-claim-guard.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT INT TERM

check() { # check <name> <expected-grep-pattern> <actual-output>
    local name="$1" pattern="$2" actual="$3"
    if grep -q "$pattern" <<<"$actual"; then
        echo "ok: $name"
    else
        echo "FAIL: $name (expected output matching '$pattern', got: $actual)" >&2
        failures=$((failures + 1))
    fi
}

# fresh copy of the real crates tree (rs files only, structure preserved)
# + the moderation SQL pin subjects (migrations 0239/0241 + the
# test-integration.sh drill fixture): rule 3g scans those three files, so
# fixtures must carry them or the clean-fixture case would false-red.
make_fixture() {
    local dest="$1"
    mkdir -p "$dest/migrations" "$dest/scripts"
    (cd "$ROOT" && find crates -name '*.rs' -print0 | tar --null -T - -cf -) \
        | (cd "$dest" && tar -xf -)
    cp "$ROOT/migrations/0239_audit_governance_outbox.sql" \
       "$ROOT/migrations/0241_governance_reconcile.sql" "$dest/migrations/"
    cp "$ROOT/scripts/test-integration.sh" "$dest/scripts/"
}

scan() { # scan <root> — run the guard, capture stdout, keep violations visible
    local root="$1"
    claim_guard_scan "$root"
}

# ── ① clean tree: the REAL tree must pass (allowlist derived from it) ──────
out="$(scan "$ROOT")"
check "clean-real-tree-zero-violations" "claim guard: 0 violation" "$out"
if grep -q '❌' <<<"$out"; then
    echo "FAIL: clean real tree produced violations:" >&2
    grep '❌' <<<"$out" >&2
    failures=$((failures + 1))
else
    echo "ok: clean-real-tree-no-violation-lines"
fi

# ── ② clean fixture: a copy must pass identically ──────────────────────────
make_fixture "$TMP/clean"
out="$(scan "$TMP/clean")"
check "clean-fixture-zero-violations" "claim guard: 0 violation" "$out"
if grep -q '❌' <<<"$out"; then
    echo "FAIL: clean fixture produced violations:" >&2
    grep '❌' <<<"$out" >&2
    failures=$((failures + 1))
else
    echo "ok: clean-fixture-no-violation-lines"
fi
if grep -q 'STALE' <<<"$out"; then
    echo "FAIL: clean fixture has stale allowlist entries (re-pin needed):" >&2
    grep 'STALE' <<<"$out" >&2
    failures=$((failures + 1))
else
    echo "ok: clean-fixture-no-stale-entries"
fi

# ── negative cases: each on its own fresh copy of the real tree ─────────────

# n1: bare literal appended to an EXISTING non-exempt file (jwt.rs) — no
#     orphan-confound (the file already exists and is mod-declared).
make_fixture "$TMP/n1"
printf 'let _unused = "iss";\n' >> "$TMP/n1/crates/aero-auth/src/jwt.rs"
out="$(scan "$TMP/n1")"
check "n1-literal-in-non-exempt-file-fails" 'CLAIM LITERAL: crates/aero-auth/src/jwt.rs:[0-9][0-9]*:"iss"' "$out"

# n2: duplicate struct definition (type single-source).
make_fixture "$TMP/n2"
printf 'pub struct ClientCredentialsClaims { bogus: u8 }\n' >> "$TMP/n2/crates/aero-auth/src/jwt.rs"
out="$(scan "$TMP/n2")"
check "n2-duplicate-struct-fails" "CLAIM TYPE: struct ClientCredentialsClaims has 2 definition" "$out"
check "n2-duplicate-struct-sites-listed" "jwt\.rs:.*struct ClientCredentialsClaims" "$out"

# n3: leaf semantic fn called from a non-consumer file (logic duplication).
make_fixture "$TMP/n3"
printf 'let _dup = check_issuer(&serde_json::Value::Null, &cfg);\n' >> "$TMP/n3/crates/aero-server/src/sso.rs"
out="$(scan "$TMP/n3")"
check "n3-usage-outside-consumers-fails" 'CLAIM USAGE:.*sso\.rs:.*check_issuer' "$out"

# n4: case-variant typ literal must be caught (case-insensitive matching).
make_fixture "$TMP/n4"
printf 'let _u = "AT+JWT";\n' >> "$TMP/n4/crates/aero-auth/src/jwt.rs"
out="$(scan "$TMP/n4")"
check "n4-typ-case-variant-fails" 'CLAIM LITERAL: crates/aero-auth/src/jwt.rs:[0-9][0-9]*:"at+jwt"' "$out"

# n5: exp/nbf are OUTSIDE the scanned vocabulary (F6) — must stay clean.
make_fixture "$TMP/n5"
printf 'let _e = "exp"; let _n = "nbf";\n' >> "$TMP/n5/crates/aero-auth/src/jwt.rs"
out="$(scan "$TMP/n5")"
check "n5-exp-nbf-excluded" "claim guard: 0 violation" "$out"

# n6: literals inside ANY `tests` path component are skipped (tests.rs
#     modules + tests/ dirs).
make_fixture "$TMP/n6"
printf 'let _t = "client_id";\n' >> "$TMP/n6/crates/aero-auth/src/oidc/tests.rs"
printf 'let _c = "iss";\n' >> "$TMP/n6/crates/aero-audit-connector/tests/claim_validation.rs"
out="$(scan "$TMP/n6")"
check "n6-tests-component-skip" "claim guard: 0 violation" "$out"

# n7: audit-flag mirror (AC4) — flag outside audit.rs is a violation.
make_fixture "$TMP/n7"
printf 'let _f = "admin.content.flag";\n' >> "$TMP/n7/crates/aero-auth/src/jwt.rs"
out="$(scan "$TMP/n7")"
check "n7-flag-outside-audit-rs-fails" 'AUDIT FLAG LITERAL:.*jwt\.rs' "$out"

# n8: allowlist entry whose file:line lost its literal → stale warning (the
#     allowlist can never silently mask a moved literal).
make_fixture "$TMP/n8"
sed -i '421s/.*/        "response_type",/' "$TMP/n8/crates/aero-server/src/sso.rs"
out="$(scan "$TMP/n8")"
check "n8-stale-allowlist-warns" "STALE ALLOWLIST ENTRY: crates/aero-server/src/sso.rs:421" "$out"

# n9: allowlist is per-line-per-literal — the SAME line with a different
#     literal is a violation, and the old pin goes stale.
make_fixture "$TMP/n9"
sed -i '441s/client_id/scope/' "$TMP/n9/crates/aero-server/src/sso.rs"
out="$(scan "$TMP/n9")"
check "n9-wrong-literal-at-pinned-line-fails" 'CLAIM LITERAL: crates/aero-server/src/sso.rs:441:"scope"' "$out"
check "n9-old-pin-goes-stale" "STALE ALLOWLIST ENTRY: crates/aero-server/src/sso.rs:441" "$out"

# n10: moderation SQL guard (3g): a drifted emission literal in the 0239
#     migration is a violation even though the leaf still matches (comment
#     lines are stripped before matching, so a comment mention cannot
#     satisfy the guard).
make_fixture "$TMP/n10sql"
sed -i "s/'action', 'admin.content.flag'/'action', 'admin.moderation.action'/" \
    "$TMP/n10sql/migrations/0239_audit_governance_outbox.sql"
out="$(scan "$TMP/n10sql")"
check "n10-sql-literal-drift-fails" "MODERATION SQL GUARD: migrations/0239_audit_governance_outbox.sql" "$out"

# n11: moderation sibling guard (3h): the sibling spelling outside the
#     leaf/allowlist is a violation (mirror of n7).
make_fixture "$TMP/n11sib"
printf 'let _s = "admin.moderation.action";\n' >> "$TMP/n11sib/crates/aero-auth/src/jwt.rs"
out="$(scan "$TMP/n11sib")"
check "n11-sibling-outside-audit-rs-fails" 'AUDIT SIBLING LITERAL:.*jwt\.rs' "$out"

# ── ⑧ exit-code fold (F5): wired truth-check exits orphan + guard ──────────
mkdir -p "$TMP/wired"
cp "$ROOT/scripts/truth-check.sh" "$ROOT/scripts/truth-check-lib.sh" "$TMP/wired/"

# positive: clean fixture → exit 0
set +e
out="$(cd "$TMP/clean" && bash "$TMP/wired/truth-check.sh" 2>&1)"
wired_exit=$?
set -e
check "fold-clean-tree-exit-zero" "结果: 0 个 ORPHAN, 3 个 UNWIRED, 0 个 CLAIM-GUARD" "$out"
if [ "$wired_exit" -ne 0 ]; then
    echo "FAIL: wired truth-check on clean tree exited $wired_exit (expected 0)" >&2
    failures=$((failures + 1))
else
    echo "ok: fold-clean-tree-exit-code-0"
fi

# negative: fixture with one injected literal → exit = orphan(0) + guard(1)
make_fixture "$TMP/n10"
printf 'let _unused = "iss";\n' >> "$TMP/n10/crates/aero-auth/src/jwt.rs"
set +e
out="$(cd "$TMP/n10" && bash "$TMP/wired/truth-check.sh" 2>&1)"
wired_exit=$?
set -e
check "fold-guard-count-reported" "结果: 0 个 ORPHAN, 3 个 UNWIRED, 1 个 CLAIM-GUARD" "$out"
if [ "$wired_exit" -ne 1 ]; then
    echo "FAIL: wired truth-check on violating tree exited $wired_exit (expected 1 = orphan 0 + guard 1)" >&2
    failures=$((failures + 1))
else
    echo "ok: fold-violation-exit-code-1"
fi

if [ "$failures" -ne 0 ]; then
    echo "✗ claim-contract guard: ${failures} regression case(s) failed" >&2
    exit 1
fi
echo "✓ claim-contract guard: all regression cases passed"
