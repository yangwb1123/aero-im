#!/usr/bin/env bash
# generate skill — regenerate engineering scaffolding (aero-cli project overview).
#
# Usage: aero-cli skill run generate
# Equivalent to snaplink's `cli.py generate` command.
# Prints project structure, gate status, and test counts.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

echo "=== Aero IM Engineering Overview ==="
echo "Generated: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
echo ""

# Workspace crates
echo "--- Workspace Crates ---"
grep '^members' Cargo.toml | tr -d '[],"' | sed 's/members = //' | tr '\n' ' ' | fmt -w 72
echo ""

# Migration count
echo "--- Migrations ---"
COUNT=$(ls migrations/*.sql 2>/dev/null | wc -l)
echo "  $COUNT migrations"
echo ""

# Gate status
echo "--- Gate Status ---"
for gate in filesize truth web deps complexity; do
    case $gate in
        filesize) SCRIPT="scripts/file-size-check.sh" ;;
        truth)    SCRIPT="scripts/truth-check.sh" ;;
        web)      SCRIPT="scripts/web-check.sh" ;;
        deps)     SCRIPT="scripts/dependency-check.sh" ;;
        complexity) SCRIPT="scripts/complexity-check.sh" ;;
    esac
    if [ -f "$SCRIPT" ]; then
        echo "  $gate: $SCRIPT (available)"
    else
        echo "  $gate: (no script)"
    fi
done
echo ""

# Test count (if results exist)
if [ -f target/debug/.test_result ]; then
    echo "--- Last Test Run ---"
    cat target/debug/.test_result
fi

echo "=== End ==="
