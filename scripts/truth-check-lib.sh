#!/usr/bin/env bash
# truth-check-lib.sh — claim-contract guard (truth-check §3, AC4 pattern).
#
# Sourceable library (mirrors scripts/b5-pin.sh): sourcing defines only
# CLAIM_ALLOWLIST / CLAIM_ALLOWED and the claim_guard_scan function — no
# output, no exit, safe under `set -euo pipefail`.
#
# Consumers:
#   * scripts/truth-check.sh — wired at migration step 5: sources this lib,
#     calls `claim_guard_scan "$(pwd)"`, folds CLAIM_GUARD_VIOLATIONS into
#     its exit code (`exit $((orphan_violations + CLAIM_GUARD_VIOLATIONS))`).
#   * scripts/test-claim-contract-guard.sh — exercises clean-tree and every
#     negative case standalone (pure bash, no database).
#
# What it polices (RFC 9068 client_credentials claim contract, single-sourced
# in crates/aero-common/src/model/client_credentials.rs):
#   1. LITERAL single-source — the 10 claim/typ spellings may appear only in
#      the leaf, in test/skip contexts, or at allowlisted file:line sites
#      (each with a true contract reason). Any other hit = hard violation.
#      The two typ literals ("at+jwt" / "application/at+jwt") match
#      case-insensitively (media types are case-insensitive, RFC 6838; the
#      validator itself uses eq_ignore_ascii_case).
#   2. TYPE single-source — `struct ClientCredentialsClaims` /
#      `ClientCredentialsTokenConfig` / `ClientCredentialsGateClaims` must
#      have exactly one definition each, in the leaf (anchored
#      `^\s*(pub(...)?\s+)?struct\s+<Name>\b` — doc comments / `use` sites
#      don't match).
#   3. USAGE single-source — call sites of the leaf semantic functions
#      (check_issuer/check_audience/check_scope/check_subject/
#      granted_scopes/subject_is_client_id/has_required_scopes/
#      is_valid_identity_component) are confined to the leaf (incl. its
#      lib.rs export surface) and the two consumers (oidc.rs / client.rs).
#      Closes the logic-duplication hole: a copied validator that *calls*
#      the leaf fns from a third file is caught even though it spells
#      nothing.
#   4. AUDIT-FLAG mirror (AC4) — "admin.content.flag" only in
#      crates/aero-common/src/model/audit.rs (makes the audit.rs doc comment
#      a fact) or at an allowlisted pin site (AUDIT_FLAG_ALLOWLIST — the
#      drill bin's contract-vocabulary pin, a deliberate second site with
#      its own regression surface).
#   5. MODERATION-SQL mirror (3g) — the emitted SQL literals
#      ('action', '<leaf>') in migrations 0239/0241 (and the drill fixture in
#      scripts/test-integration.sh) must match the leaf constant byte-for-byte
#      on NON-comment lines: rule 3d scans Rust only, so the SQL emission
#      sites are the load-bearing pin for the *emitted* token. A leaf flip
#      without the coordinated SQL flip reds here.
#   6. AUDIT-SIBLING mirror (3h) — the contract pair's sibling spelling
#      "admin.moderation.action" gets the same single-source scan as 3d
#      (same skip rules + same AUDIT_FLAG_ALLOWLIST — both pin sites contain
#      the sibling; a future production `.rs` sibling drift reds).
#
# Skip rules (checked before the allowlist):
#   * whole-file exemptions: the leaf file itself (its constants/tests are
#     the single source) and crates/aero-audit-connector/src/stub.rs (test
#     double — fixture JSON is data, not logic);
#   * ANY path component named `tests` or `tests.rs` (covers crates/*/tests/
#     dirs, src/**/tests.rs modules and src/**/tests/*.rs submodules);
#   * comment-led lines (`^\s*//` — covers `//!`/`///` docs).
#
# Allowlist entries are file:line:literal keys pinned to the live tree at
# derivation time (see the re-spec in
# docs/design/2026-08-08-aero-auth-client-credentials-claim-contract-leaf.design.md
# §1.5). A stale entry (file:line no longer containing the literal) is a
# warning forcing a re-pin — the allowlist can never silently mask a moved
# literal.

# ───────────────────────────────────────────────────────────────────────────
# Allowlist — every entry is a DIFFERENT wire contract than the unified
# four-claim contract. Key format: <path>:<line>:<literal> (literal
# lowercased — the typ entries match case-insensitively).
# ───────────────────────────────────────────────────────────────────────────
CLAIM_ALLOWLIST=(
    # aero-auth oidc.rs — id_token (OIDC Core / RFC 7519) contract + Debug
    # labels. The cc-token sites (pre-migration :429/:430/:453) are NOT here:
    # they now reference CLAIM_* / TOKEN_TYPE_* constants.
    'crates/aero-auth/src/oidc.rs:175:"sub"'   # Debug formatter field label — OidcClaims (id_token struct) Debug impl, PII redaction
    'crates/aero-auth/src/oidc.rs:184:"iat"'   # Debug formatter field label — same impl
    'crates/aero-auth/src/oidc.rs:367:"iss"'   # id_token set_required_spec_claims — RFC 7519/OIDC Core id_token contract (different wire contract)
    'crates/aero-auth/src/oidc.rs:367:"aud"'   # id_token set_required_spec_claims — same
    'crates/aero-auth/src/oidc.rs:367:"sub"'   # id_token set_required_spec_claims — same
    # audit connector client.rs — RFC 6749 §4.4 token-request form param in
    # request_token (wire contract, not claims validation).
    'crates/aero-audit-connector/src/client.rs:491:"scope"'
    # relay.rs — #[tokio::test] fixture JSON (claim-drift test data).
    'crates/aero-audit-connector/src/relay.rs:536:"iss"'
    'crates/aero-audit-connector/src/relay.rs:537:"aud"'
    'crates/aero-audit-connector/src/relay.rs:538:"scope"'
    'crates/aero-audit-connector/src/relay.rs:539:"sub"'
    'crates/aero-audit-connector/src/relay.rs:540:"client_id"'
    # im-core governance_drill_tests/heartbeat.rs — S4 drill fixture: the
    # scope-missing client-credentials token (B5-4 ScopeRejected path). The
    # fixture must carry the literal claim names because it exercises the
    # wire contract the leaf's check_* fns validate (the drill asserts the
    # typed-gate-before-check_scope order with a valid sub/client_id + a
    # non-granting scope).
    'crates/aero-im-core/src/db_tests/governance_drill_tests/heartbeat.rs:332:"iss"'
    'crates/aero-im-core/src/db_tests/governance_drill_tests/heartbeat.rs:333:"aud"'
    'crates/aero-im-core/src/db_tests/governance_drill_tests/heartbeat.rs:334:"scope"'
    'crates/aero-im-core/src/db_tests/governance_drill_tests/heartbeat.rs:335:"sub"'
    'crates/aero-im-core/src/db_tests/governance_drill_tests/heartbeat.rs:336:"client_id"'
    # jwt.rs — participant JWT shape assertion deliberately names the
    # forbidden audit scope key; it is a negative wire-contract pin, not a
    # second claim implementation.
    'crates/aero-auth/src/jwt.rs:306:"scope"'
    # audit connector metrics.rs — R-D2 §10.1 runbook counter label
    # vocabularies + rejection-reason classifier (bounded label strings for
    # aero_audit_token_rejections_total / _delivery_outcomes_total; NOT
    # claim-validation logic — the leaf's check_* fns stay the single
    # authority, usage-rule 3 pinned to oidc.rs/client.rs only).
    'crates/aero-audit-connector/src/metrics.rs:31:"scope"'   # TOKEN_REJECT_REASONS label vocabulary
    'crates/aero-audit-connector/src/metrics.rs:93:"scope"'  # classify_token_rejection scope-match arm
    'crates/aero-audit-connector/src/metrics.rs:94:"scope"'  # classify_token_rejection scope label
    'crates/aero-audit-connector/src/metrics.rs:95:"iss"'    # classify_token_rejection claims-match arm
    'crates/aero-audit-connector/src/metrics.rs:96:"aud"'    # same
    'crates/aero-audit-connector/src/metrics.rs:97:"sub"'    # same
    # extractor.rs — #[cfg(test)] Claims test-struct fixture field values.
    'crates/aero-auth/src/extractor.rs:173:"jti"'
    'crates/aero-auth/src/extractor.rs:191:"jti"'
    # sso.rs — OIDC authorization-code (RFC 6749 §4.1) form params + IdP
    # response `iss` parse (OIDC Core §3.1.3.7) — a different OIDC flow.
    'crates/aero-server/src/sso.rs:385:"client_id"' # RESERVED auth-code param list
    'crates/aero-server/src/sso.rs:387:"scope"'     # RESERVED auth-code param list
    'crates/aero-server/src/sso.rs:405:"client_id"' # authorization-code request append_pair
    'crates/aero-server/src/sso.rs:407:"scope"'     # authorization-code request append_pair
    'crates/aero-server/src/sso.rs:545:"iss"'       # IdP response form-param parse
    # integrations.rs — account-summary audit detail plus #[test] fixture
    # JSON (CreateInstallationReq payloads). The production field is an
    # audit-detail schema key, not client-credential claim validation.
    'crates/aero-server/src/integrations.rs:461:"client_id"'
    'crates/aero-server/src/integrations/tests.rs:62:"client_id"'
    'crates/aero-server/src/integrations/tests.rs:72:"client_id"'
    'crates/aero-server/src/integrations/tests.rs:103:"client_id"'
    # snaplink_commercial/http.rs — RFC 6749 §4.4 token-request form param
    # (v1 Snaplink client). "冻结面" is policy; the contract reason is the
    # §4.4 wire param.
    'crates/aero-server/src/snaplink_commercial/http.rs:234:"scope"'
    # vault blob store — Debug formatter field labels (:60/:62) + vault OAuth
    # token-request form param (:291, RFC 6749 §4.4).
    'crates/aero-storage/src/aero_vault_blob_store.rs:60:"client_id"'
    'crates/aero-storage/src/aero_vault_blob_store.rs:62:"scope"'
    'crates/aero-storage/src/aero_vault_blob_store.rs:291:"scope"'
    # admin_revoke.rs — audit-event payload field name ("session.revoked"
    # audit JSON schema), NOT a vault form param. Removed with the B5-1 auth
    # slice: the legacy append's `"scope": "participant_global"` payload
    # field was replaced by the governance pair writer (detail
    # `{"admin_revoked": true}`) — no `scope` literal remains in the file.
    # blob.rs / blob_gc.rs — expect("scope") panic-message strings inside
    # #[tokio::test] fns, NOT production vault params.
    'crates/aero-storage/src/blob.rs:876:"scope"'
    'crates/aero-storage/src/blob_gc.rs:289:"scope"'
    # integration*.rs — audit-event payload field name (:234,
    # "integration.installation.created" audit JSON schema) + error-message
    # field-name args to validate_identity_component (:417/:546/:295/:546).
    'crates/aero-storage/src/integration.rs:234:"client_id"'
    'crates/aero-storage/src/integration.rs:417:"client_id"'
    'crates/aero-storage/src/integration.rs:546:"client_id"'
    'crates/aero-storage/src/integration/support.rs:295:"client_id"'
    'crates/aero-storage/src/integration/machine.rs:546:"client_id"'
    # pat.rs — PAT token-response JSON field name "scopes" (plural; different
    # wire contract than the RFC 9068 singular "scope" claim — substring-guard
    # false positive on the token-response plural field).
    'crates/aero-storage/src/pat.rs:131:"scopes"'
    'crates/aero-storage/src/pat.rs:443:"scopes"'
    # aero-server/pat.rs — the B5-1 PAT-pair detail carries the same
    # token-response plural "scopes" field (`json!({ "scopes": scopes })`).
    'crates/aero-server/src/pat.rs:157:"scopes"'
    # db_tests/auth.rs — B5-1 auth-slice parity fixtures reuse the same
    # token-response plural "scopes" field (never the RFC 9068 singular claim).
    'crates/aero-storage/src/audit_governance/db_tests/auth.rs:248:"scopes"'
    'crates/aero-storage/src/audit_governance/db_tests/auth.rs:263:"scopes"'
    'crates/aero-storage/src/audit_governance/db_tests/auth.rs:572:"scopes"'
    # live.rs — chat-line label "sub" in #[tokio::test] (pure vocabulary
    # false positive: a subscriber/non-subscriber chat line, not a claim).
    'crates/aero-storage/src/live.rs:584:"sub"'
)

# Whole-file exemptions (skip rules 1).
CLAIM_LEAF_FILE="crates/aero-common/src/model/client_credentials.rs"
CLAIM_STUB_FILE="crates/aero-audit-connector/src/stub.rs"
CLAIM_AUDIT_FILE="crates/aero-common/src/model/audit.rs"

# AUDIT-FLAG allowlist (rules 3d + 3h) — deliberate, contract-mandated
# second sites for the moderation outbound vocabulary literals
# ("admin.content.flag" and its documented sibling
# "admin.moderation.action"). Key format: <path>:<line> (both literals are
# fixed for these rules; the leaf is the single definition point). The
# drill bin's contract-vocabulary pair is the destructive-gate direction's
# product (R3): the pair is hardcoded, NOT derived from the leaf, so a leaf
# flip cannot auto-follow and silently kill the pin — the seed-time
# EXACT-equality bail on the pair's index 0, the
# `emitted_spelling_is_the_pair_lock` unit test, and rule 3g (SQL literals)
# are the pin's own regression surface. Stale entries (file:line no longer
# containing the literals) warn → re-pin, exactly like the CLAIM_ALLOWLIST
# mechanism.
AUDIT_FLAG_ALLOWLIST=(
    'crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs:95'
    'crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs:658'
    # aero-server metrics.rs — B5-4 sampler PG-gated test fixtures: the
    # seeded `audit_governance_outbox` rows must carry the moderation
    # outbound vocabulary literal to exercise the Q3/Q4 mirror SQL with
    # realistic payloads (the sampler asserts buckets/ages, not the action
    # strings themselves).
    # metrics.rs — audit-outbox sampler db_tests fixtures ("action":
    # MODERATION_OUTBOUND_ACTION literal in seeded governance envelopes —
    # test-only, mirrors the relay.rs fixture-allowlist precedent).
    'crates/aero-server/src/metrics/tests.rs:407'
    'crates/aero-server/src/metrics/tests.rs:422'
    'crates/aero-server/src/metrics/tests.rs:489'
    # aero-ai governance plain fixture: the literal tuple is deliberately
    # independent of the leaf constants so a token/DDL drift fails early.
    'crates/aero-ai/src/governance.rs:362'
)

# L1 allowlist-token guard (rule 3e) — F2 closure for migration 0242's
# SQL-side allowlist (`aero_enqueue_l1_aggregate_audit`) + the 0246
# message-recall allowlist (B5-1 outbox enqueue coverage). The four action
# tokens are legal ONLY in the leaf (`CLAIM_AUDIT_FILE`, the single Rust
# definition point) plus allowlisted pin sites; the 0242/0246 SQL literals
# are
# pinned by the aero-storage `l1_window_aggregates_*` db_tests (G2: truth-
# check scans `crates --glob '*.rs'` only, so the SQL side is unpoliced here;
# the db_test recomputes the window key from the leaf consts and breaks on
# any drift). Tests components are skipped (3a/3c precedent): governance.rs
# test literals were rewritten to the consts anyway, so a bare token in
# production code is a hard violation while test spellings stay lint-clean.
# `"aero-im.source"` (AUDIT_SOURCE_SYSTEM) is deliberately NOT in this
# guard: it is the deployment-wide source-system value legitimately spelled
# in ~49 sites across 11 files (connector config/JWT claims/drill configs/
# binding fixtures); its 0242-SQL pin is the AC2 db_test field assertion
# (`payload->>'source_system' == AUDIT_SOURCE_SYSTEM`), not a literal scan.
L1_TOKEN_LITERALS_CS=( '"message.create"' '"message.edit"' '"message.batch"' '"message.recalled"' '"message.deleted"' )
L1_TOKEN_ALLOWLIST=()

# Room allowlist-token guard (rule 3f) — N3 closure for migration 0245's
# SQL-side allowlist (`aero_enqueue_room_audit`). The two room tokens are
# legal ONLY in the leaf (`CLAIM_AUDIT_FILE`, the single Rust definition
# point); `ROOM_TOKEN_ALLOWLIST` is EMPTY — there are no pin sites today
# (governance.rs / audit_governance.rs test literals were rewritten to the
# consts), and the stale-entry loop below warns on drift if one is ever
# added. Same skip rules as 3e (tests components + comments skipped; SQL
# side unpoliced — the 0245 literals are pinned by the aero-storage
# `room_lane_outbox_parity` db_test recomputed from the leaf consts). A
# future Rust producer emitting a bare room token reds here — the consts are
# the only legal spelling (trigger-only ownership, 0245 file header).
ROOM_TOKEN_LITERALS_CS=( '"room.create"' '"room.archived"' )
ROOM_TOKEN_ALLOWLIST=()

# Usage-scan allowlist: leaf definition site + its lib.rs export surface +
# the two consumers (+ aero-auth's id_token validator module, which shares
# is_valid_identity_component for its own contract). Any other call site =
# logic-duplication risk.
CLAIM_USAGE_ALLOWED=(
    "crates/aero-common/src/model/client_credentials.rs"
    "crates/aero-common/src/lib.rs"
    "crates/aero-auth/src/oidc.rs"
    "crates/aero-auth/src/oidc/id_token.rs"
    "crates/aero-audit-connector/src/client.rs"
)

# The 8 case-sensitive literals (quoted so `"scope"` never matches inside
# `"scopes"`), then the 2 typ literals matched case-insensitively.
CLAIM_LITERALS_CS=( '"iss"' '"aud"' '"scope"' '"scopes"' '"sub"' '"client_id"' '"iat"' '"jti"' )
CLAIM_LITERALS_CI=( '"at+jwt"' '"application/at+jwt"' )

CLAIM_USAGE_PATTERN='\b(check_issuer|check_audience|check_scope|check_subject|granted_scopes|subject_is_client_id|has_required_scopes|is_valid_identity_component)\b'

declare -A CLAIM_ALLOWED=()
for _entry in "${CLAIM_ALLOWLIST[@]}"; do
    CLAIM_ALLOWED["$_entry"]=1
done

declare -A AUDIT_FLAG_ALLOWED=()
for _entry in "${AUDIT_FLAG_ALLOWLIST[@]}"; do
    AUDIT_FLAG_ALLOWED["$_entry"]=1
done

declare -A L1_TOKEN_ALLOWED=()
for _entry in "${L1_TOKEN_ALLOWLIST[@]}"; do
    L1_TOKEN_ALLOWED["$_entry"]=1
done

declare -A ROOM_TOKEN_ALLOWED=()
for _entry in "${ROOM_TOKEN_ALLOWLIST[@]}"; do
    ROOM_TOKEN_ALLOWED["$_entry"]=1
done

CLAIM_GUARD_VIOLATIONS=0

# skip_tests_component <path> — 1 if ANY path component is `tests` or
# `tests.rs` (top-level tests/ dirs, src/**/tests.rs modules, src/**/tests/*).
skip_tests_component() {
    local path="$1" comp
    local -a comps
    IFS='/' read -r -a comps <<< "$path"
    for comp in "${comps[@]}"; do
        [ "$comp" = "tests" ] && return 0
        [ "$comp" = "tests.rs" ] && return 0
    done
    return 1
}

# is_comment_line <root> <path> <line> — 1 if the line is comment-led
# (`^\s*//`, covers `//!`/`///`).
is_comment_line() {
    local root="$1" path="$2" line="$3" text
    text=$(sed -n "${line}p" "$root/$path" 2>/dev/null || true)
    printf '%s\n' "$text" | grep -qE '^[[:space:]]*//'
}

# claim_guard_scan <root> — runs the five scans over <root>/crates, prints
# ❌ violation / ⚠️ stale-entry lines, sets CLAIM_GUARD_VIOLATIONS, returns 0.
claim_guard_scan() {
    local root="$1"
    local literal_violations=0 type_violations=0 usage_violations=0 flag_violations=0
    local l1_violations=0 room_violations=0 sql_violations=0 sibling_violations=0
    local hits hit path rest line lit lit_lc key h n
    local -a cs_args ci_args
    for l in "${CLAIM_LITERALS_CS[@]}"; do cs_args+=(-e "$l"); done
    for l in "${CLAIM_LITERALS_CI[@]}"; do ci_args+=(-e "$l"); done

    if ! command -v rg >/dev/null 2>&1; then
        echo "  ❌ CLAIM GUARD: ripgrep (rg) not found — scan cannot run (fail-closed)"
        CLAIM_GUARD_VIOLATIONS=1
        return 0
    fi

    echo "--- 3. claim-contract guard（字面量/类型/usage 单源, AC4）---"

    # ── 3a. literal single-source ────────────────────────────────────────
    # rg runs from $root with the relative `crates` path so hit paths match
    # the allowlist keys (absolute vs relative would never compare equal).
    hits=$(
        cd "$root" && rg -n -o -F "${cs_args[@]}" crates --glob '*.rs' 2>/dev/null || true
        cd "$root" && rg -n -o -i -F "${ci_args[@]}" crates --glob '*.rs' 2>/dev/null || true
    )
    hits=$(printf '%s\n' "$hits" | sort -u)
    while IFS= read -r hit; do
        [ -z "$hit" ] && continue
        path="${hit%%:*}"
        rest="${hit#*:}"
        line="${rest%%:*}"
        lit="${rest#*:}"
        lit_lc=$(printf '%s' "$lit" | tr 'A-Z' 'a-z')
        # skip rules
        [ "$path" = "$CLAIM_LEAF_FILE" ] && continue
        [ "$path" = "$CLAIM_STUB_FILE" ] && continue
        skip_tests_component "$path" && continue
        is_comment_line "$root" "$path" "$line" && continue
        if [ -n "${CLAIM_ALLOWED["$path:$line:$lit_lc"]:-}" ]; then
            continue
        fi
        echo "  ❌ CLAIM LITERAL: $path:$line:$lit_lc (not allowlisted — see scripts/truth-check-lib.sh for contract reasons)"
        literal_violations=$((literal_violations + 1))
    done <<< "$hits"

    # ── 3b. type single-source (anchored struct regex) ────────────────────
    # ClientCredentialsGateClaims is the connector's minimal typed-gate
    # struct (delta ④ resolution) — same single-source rule as the other two.
    for t in ClientCredentialsClaims ClientCredentialsTokenConfig ClientCredentialsGateClaims; do
        hits=$(cd "$root" && rg -n -e "^\s*(pub(\([^)]*\))?\s+)?struct\s+${t}\b" crates --glob '*.rs' 2>/dev/null || true)
        n=0
        while IFS= read -r h; do
            [ -z "$h" ] && continue
            n=$((n + 1))
        done <<< "$hits"
        if [ "$n" -eq 1 ] && [[ "$hits" == "$CLAIM_LEAF_FILE:"* ]]; then
            : # exactly one definition, in the leaf — clean
        else
            echo "  ❌ CLAIM TYPE: struct $t has $n definition(s) (must be exactly 1, inside $CLAIM_LEAF_FILE):"
            while IFS= read -r h; do
                [ -z "$h" ] && continue
                echo "      $h"
            done <<< "$hits"
            type_violations=$((type_violations + 1))
        fi
    done

    # ── 3c. usage single-source (logic-duplication hole) ──────────────────
    hits=$(cd "$root" && rg -n -e "$CLAIM_USAGE_PATTERN" crates --glob '*.rs' 2>/dev/null || true)
    while IFS= read -r h; do
        [ -z "$h" ] && continue
        path="${h%%:*}"
        rest="${h#*:}"
        line="${rest%%:*}"
        skip_tests_component "$path" && continue
        is_comment_line "$root" "$path" "$line" && continue
        if [[ " ${CLAIM_USAGE_ALLOWED[*]} " == *" $path "* ]]; then
            continue
        fi
        echo "  ❌ CLAIM USAGE: $h (leaf semantic fn called outside leaf/consumers — logic duplication risk)"
        usage_violations=$((usage_violations + 1))
    done <<< "$hits"

    # ── 3d. audit-flag mirror (AC4) ───────────────────────────────────────
    hits=$(cd "$root" && rg -n -F '"admin.content.flag"' crates --glob '*.rs' 2>/dev/null || true)
    while IFS= read -r h; do
        [ -z "$h" ] && continue
        path="${h%%:*}"
        rest="${h#*:}"
        line="${rest%%:*}"
        if [ "$path" != "$CLAIM_AUDIT_FILE" ] \
            && [ -z "${AUDIT_FLAG_ALLOWED["$path:$line"]:-}" ]; then
            echo "  ❌ AUDIT FLAG LITERAL: $h (only legal in $CLAIM_AUDIT_FILE or an allowlisted pin site)"
            flag_violations=$((flag_violations + 1))
        fi
    done <<< "$hits"

    # ── 3e. L1 allowlist-token guard (F2 closure; 0242 SQL-side allowlist) ─
    hits=$(cd "$root" && rg -n -o -F "${L1_TOKEN_LITERALS_CS[@]}" crates --glob '*.rs' 2>/dev/null || true)
    hits=$(printf '%s\n' "$hits" | sort -u)
    while IFS= read -r h; do
        [ -z "$h" ] && continue
        path="${h%%:*}"
        rest="${h#*:}"
        line="${rest%%:*}"
        lit="${rest#*:}"
        # skip rules (3a mirror): leaf is the single definition point;
        # tests components may spell tokens; comments are not code.
        [ "$path" = "$CLAIM_AUDIT_FILE" ] && continue
        skip_tests_component "$path" && continue
        is_comment_line "$root" "$path" "$line" && continue
        if [ -n "${L1_TOKEN_ALLOWED["$path:$line:$lit"]:-}" ]; then
            continue
        fi
        echo "  ❌ L1 TOKEN LITERAL: $path:$line:$lit (0242 allowlist token outside the leaf — a rename here silently drops aggregation)"
        l1_violations=$((l1_violations + 1))
    done <<< "$hits"

    # ── 3f. Room allowlist-token guard (N3 closure; 0245 SQL-side
    # allowlist) — mirror 3e exactly. Empty allowlist: any bare room token
    # outside the leaf is a hard violation (rule 3f); the 0245 SQL literals
    # are pinned by the room_lane_outbox_parity db_test (G2 closure). ──
    hits=$(cd "$root" && rg -n -o -F "${ROOM_TOKEN_LITERALS_CS[@]}" crates --glob '*.rs' 2>/dev/null || true)
    hits=$(printf '%s\n' "$hits" | sort -u)
    while IFS= read -r h; do
        [ -z "$h" ] && continue
        path="${h%%:*}"
        rest="${h#*:}"
        line="${rest%%:*}"
        lit="${rest#*:}"
        # skip rules (3a/3e mirror): leaf is the single definition point;
        # tests components may spell tokens; comments are not code.
        [ "$path" = "$CLAIM_AUDIT_FILE" ] && continue
        skip_tests_component "$path" && continue
        is_comment_line "$root" "$path" "$line" && continue
        if [ -n "${ROOM_TOKEN_ALLOWED["$path:$line:$lit"]:-}" ]; then
            continue
        fi
        echo "  ❌ ROOM TOKEN LITERAL: $path:$line:$lit (0245 allowlist token outside the leaf — Rust must never write the outbox for room tokens; use the leaf consts)"
        room_violations=$((room_violations + 1))
    done <<< "$hits"

    # ── 3g. moderation outbound SQL-literal guard (emitted-token pin) ────
    # The 0239 trigger / 0241 reconciler hardcode the outbound moderation
    # token in SQL (and test-integration.sh seeds a drill fixture with it);
    # rule 3d scans Rust only, so the emitted token's SQL spellings are
    # statically unpoliced. Extract the leaf value once and require the
    # exact `'action', '<leaf>'` pair in each file on a NON-comment line
    # (a comment mentioning the literal must not satisfy the guard). A leaf
    # flip without the coordinated SQL flip reds here.
    leaf_moderation_action=$(sed -n 's/^pub const MODERATION_OUTBOUND_ACTION: &str = "\([^"]*\)";/\1/p' "$root/$CLAIM_AUDIT_FILE")
    leaf_moderation_lines=$(printf '%s\n' "$leaf_moderation_action" | grep -c . || true)
    if [ "$leaf_moderation_lines" -ne 1 ]; then
        echo "  ❌ MODERATION SQL GUARD: leaf MODERATION_OUTBOUND_ACTION extraction yielded $leaf_moderation_lines line(s) (must be exactly 1)"
        sql_violations=$((sql_violations + 1))
    else
        for sql_file in migrations/0239_audit_governance_outbox.sql migrations/0241_governance_reconcile.sql scripts/test-integration.sh; do
            # Do not use grep -q in the consumer: this file is sourced under
            # `pipefail`, and an early quit can turn the producer's SIGPIPE
            # into a false negative when the fixture is large.
            if ! grep -v '^[[:space:]]*--' "$root/$sql_file" 2>/dev/null | grep -F "'action', '$leaf_moderation_action'" >/dev/null; then
                echo "  ❌ MODERATION SQL GUARD: $sql_file lacks the exact emitted literal 'action', '$leaf_moderation_action' (leaf MODERATION_OUTBOUND_ACTION) on a non-comment line"
                sql_violations=$((sql_violations + 1))
            fi
        done
    fi

    # ── 3h. moderation sibling-literal scan (symmetric to 3d) ─────────────
    # The contract pair's sibling spelling gets its first scan: any
    # production `.rs` site spelling "admin.moderation.action" outside the
    # leaf/allowlist is a violation (same skip rules as 3d's documented set
    # — leaf + tests components + comment lines — plus the same AUDIT_FLAG
    # allowlist: both drill pin sites contain the sibling). A future sibling
    # drift in production code reds here; the claim_validation negative-twin
    # fixture lives in a tests/ dir and is skipped by design.
    hits=$(cd "$root" && rg -n -F '"admin.moderation.action"' crates --glob '*.rs' 2>/dev/null || true)
    while IFS= read -r h; do
        [ -z "$h" ] && continue
        path="${h%%:*}"
        rest="${h#*:}"
        line="${rest%%:*}"
        [ "$path" = "$CLAIM_AUDIT_FILE" ] && continue
        skip_tests_component "$path" && continue
        is_comment_line "$root" "$path" "$line" && continue
        if [ -z "${AUDIT_FLAG_ALLOWED["$path:$line"]:-}" ]; then
            echo "  ❌ AUDIT SIBLING LITERAL: $h (sibling spelling only legal in $CLAIM_AUDIT_FILE or an allowlisted pin site)"
            sibling_violations=$((sibling_violations + 1))
        fi
    done <<< "$hits"

    # ── stale allowlist entries (drift → re-pin) ──────────────────────────
    for entry in "${CLAIM_ALLOWLIST[@]}"; do
        path="${entry%%:*}"
        rest="${entry#*:}"
        line="${rest%%:*}"
        expected="${rest#*:}"
        if ! sed -n "${line}p" "$root/$path" 2>/dev/null | grep -qiF "$expected"; then
            echo "  ⚠️  STALE ALLOWLIST ENTRY: $entry (file:line no longer contains the literal — re-pin)"
        fi
    done
    for entry in "${AUDIT_FLAG_ALLOWLIST[@]}"; do
        path="${entry%%:*}"
        line="${entry#*:}"
        if ! sed -n "${line}p" "$root/$path" 2>/dev/null | grep -qiF '"admin.content.flag"'; then
            echo "  ⚠️  STALE AUDIT-FLAG ALLOWLIST ENTRY: $entry (file:line no longer contains the literal — re-pin)"
        fi
    done

    CLAIM_GUARD_VIOLATIONS=$((literal_violations + type_violations + usage_violations + flag_violations + l1_violations + room_violations + sql_violations + sibling_violations))
    echo "  claim guard: ${CLAIM_GUARD_VIOLATIONS} violation(s) (literal=${literal_violations} type=${type_violations} usage=${usage_violations} flag=${flag_violations} l1=${l1_violations} room=${room_violations} sql=${sql_violations} sibling=${sibling_violations})"
    return 0
}
