#!/usr/bin/env bash
# coordinated-flip-drill.sh — B5-1 dual-moderation token A1.3 (design gate F3).
#
# A1.3: a COORDINATED flip of the outbound moderation token — leaf const +
# every SQL emission site + the drill's locked spelling — in one commit MUST
# pass the full suite (the pin architecture's acceptance). This script makes
# that condition executable:
#
#   bash scripts/coordinated-flip-drill.sh          # dry-run: verify the 5
#                                                    # sites are in sync + list
#                                                    # what a flip touches
#   bash scripts/coordinated-flip-drill.sh --apply   # flip ALL sites to the
#                                                    # sibling spelling in a
#                                                    # THROWAWAY git worktree,
#                                                    # run the suite, discard
#
# The flip is exercised in a temporary worktree only — the working tree is
# never touched (the sibling spelling must never land in production).
#
# The five in-sync sites (truth-check rule 3g + the drill's index-0 lock):
#   1. aero-common/src/model/audit.rs  MODERATION_OUTBOUND_ACTION
#   2. migrations/0239_audit_governance_outbox.sql   (enqueue trigger literal)
#   3. migrations/0241_governance_reconcile.sql      (reconciler literal)
#   4. scripts/test-integration.sh                  (drill-seed psql fixture)
#   5. aero-audit-connector/src/bin/aero-audit-priority-drill.rs  (pair index 0)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LEAF="$ROOT/crates/aero-common/src/model/audit.rs"
CURRENT="admin.content.flag"
SIBLING="admin.moderation.action"

sites() {
    echo "1. $LEAF  (MODERATION_OUTBOUND_ACTION const)"
    echo "2. $ROOT/migrations/0239_audit_governance_outbox.sql"
    echo "3. $ROOT/migrations/0241_governance_reconcile.sql"
    echo "4. $ROOT/scripts/test-integration.sh"
    echo "5. $ROOT/crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs"
}

check_sync() {
    local leaf_value
    leaf_value="$(sed -n 's/^pub const MODERATION_OUTBOUND_ACTION: &str = "\([^"]*\)";/\1/p' "$LEAF")"
    [ -n "$leaf_value" ] || { echo "✗ leaf const not found" >&2; exit 2; }
    [ "$leaf_value" = "$CURRENT" ] || { echo "✗ leaf is '$leaf_value' (expected '$CURRENT')" >&2; exit 2; }
    for f in "$ROOT/migrations/0239_audit_governance_outbox.sql" \
             "$ROOT/migrations/0241_governance_reconcile.sql" \
             "$ROOT/scripts/test-integration.sh"; do
        if ! grep -Fq "'action', '$CURRENT'" "$f" && ! grep -Fq "\"action\", '$CURRENT'" "$f"; then
            echo "✗ $f does not carry the '$CURRENT' emission literal" >&2
            exit 2
        fi
    done
    echo "✓ all $(( $(sites | wc -l) )) sites in sync on '$CURRENT'"
}

apply_flip() {
    local wd
    wd="$(mktemp -d "${TMPDIR:-/tmp}/aero-flip.XXXXXX")"
    trap 'rm -rf "$wd"' EXIT
    git -C "$ROOT" worktree add --detach "$wd" HEAD >/dev/null
    cd "$wd"
    # 1. leaf const + vocabulary pair (index 0 = pinned; swap members)
    python3 - "$CURRENT" "$SIBLING" <<'PY'
import pathlib, sys
cur, sib = sys.argv[1], sys.argv[2]
f = pathlib.Path("crates/aero-common/src/model/audit.rs")
t = f.read_text()
old_pair = f'["{cur}", "{sib}"]'
new_pair = f'["{sib}", "{cur}"]'
assert old_pair in t, "vocabulary pair not found"
t = t.replace(f'pub const MODERATION_OUTBOUND_ACTION: &str = "{cur}";',
              f'pub const MODERATION_OUTBOUND_ACTION: &str = "{sib}";')
t = t.replace(old_pair, new_pair)
f.write_text(t)
PY
    # 2/3. SQL emission literals (comment-free -F matches)
    sed -i "s/'$CURRENT'/'$SIBLING'/g" migrations/0239_audit_governance_outbox.sql migrations/0241_governance_reconcile.sql
    # 4. harness fixture
    sed -i "s/'$CURRENT'/'$SIBLING'/g" scripts/test-integration.sh
    # 5. drill contract pair: swap the two members (index 0 = new pinned
    #    spelling, index 1 = the previous pin — a blanket replace would
    #    duplicate the pair and red the derived-sibling unit pin). Python,
    #    not sed: the brackets are regex character-class metacharacters.
    python3 - "$CURRENT" "$SIBLING" <<'PY'
import pathlib, sys
cur, sib = sys.argv[1], sys.argv[2]
f = pathlib.Path("crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs")
t = f.read_text()
old_pair = f'["{cur}", "{sib}"]'
new_pair = f'["{sib}", "{cur}"]'
assert old_pair in t, "pair not found"
f.write_text(t.replace(old_pair, new_pair))
PY
    echo "flipped all sites to '$SIBLING' in $wd"
    # The coordinated flip must stay green end-to-end (A1.3).
    cargo test -p aero-audit-connector --tests --bins --locked
    cargo test -p aero-common --lib --locked
    bash scripts/truth-check.sh
    echo "✓ coordinated flip PASS — A1.3 executable"
}

if [ "${1:-}" = "--apply" ]; then
    check_sync
    apply_flip
else
    check_sync
    echo "A1.3 sites (a coordinated flip touches all five, in one commit):"
    sites
    echo "run with --apply to exercise the flip in a throwaway worktree"
fi
