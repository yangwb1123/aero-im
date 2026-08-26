#!/usr/bin/env bash
# Incremental rustfmt gate. Files that were formatted on the Git baseline (and
# all new Rust files) must remain formatted. Pre-existing formatting debt is
# reported but does not force an unrelated change to rewrite the whole file.

set -euo pipefail

ROOT="${AERO_FORMAT_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
TMP="$(mktemp -d -t aero-fmt-check.XXXXXX)"

cleanup() {
    rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

cd "$ROOT"

if ! command -v rustfmt >/dev/null 2>&1; then
    echo "format-incremental: rustfmt is required" >&2
    exit 1
fi
if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    echo "format-incremental: $ROOT is not a Git worktree" >&2
    exit 1
fi

requested_base="${AERO_FORMAT_BASE_REF:-}"
if [[ -z "$requested_base" && -n "${GITHUB_BASE_REF:-}" ]]; then
    requested_base="origin/${GITHUB_BASE_REF}"
fi
if [[ -z "$requested_base" ]]; then
    requested_base=HEAD
fi
if [[ "$requested_base" =~ ^0+$ ]]; then
    if git rev-parse --verify --quiet HEAD^ >/dev/null; then
        requested_base=HEAD^
    else
        requested_base=HEAD
    fi
fi
if ! git rev-parse --verify --quiet "${requested_base}^{commit}" >/dev/null; then
    echo "format-incremental: unknown baseline: $requested_base" >&2
    exit 1
fi

compare_base="$requested_base"
if [[ "$requested_base" != HEAD ]]; then
    if ! compare_base="$(git merge-base "$requested_base" HEAD)"; then
        echo "format-incremental: cannot find merge-base for $requested_base" >&2
        exit 1
    fi
fi

mapfile -t changed_files < <(
    {
        git diff --name-only --diff-filter=ACMR "$compare_base" -- crates
        git ls-files --others --exclude-standard -- crates
    } | awk '/\.rs$/' | sort -u
)

checked=0
legacy_debt=0
violations=0
for file in "${changed_files[@]}"; do
    [[ -f "$file" ]] || continue
    checked=$((checked + 1))
    current_output="$TMP/current-$checked.log"
    if rustfmt --edition 2021 --check "$file" >"$current_output" 2>&1; then
        continue
    fi

    baseline_was_formatted=1
    if git cat-file -e "$compare_base:$file" 2>/dev/null; then
        baseline_file="$TMP/baseline-$checked.rs"
        git show "$compare_base:$file" >"$baseline_file"
        if ! rustfmt --edition 2021 --check "$baseline_file" >/dev/null 2>&1; then
            baseline_was_formatted=0
        fi
    fi

    if (( baseline_was_formatted == 0 )); then
        legacy_debt=$((legacy_debt + 1))
        echo "  WARN legacy rustfmt debt: $file"
        continue
    fi

    violations=$((violations + 1))
    echo "  ERROR new rustfmt drift: $file" >&2
    sed -n '1,80p' "$current_output" >&2
done

if (( violations > 0 )); then
    printf 'format-incremental: FAIL (%d checked, %d violations, %d legacy warnings)\n' \
        "$checked" "$violations" "$legacy_debt" >&2
    exit 1
fi

printf 'format-incremental: PASS (%d checked, %d legacy warnings; baseline %s)\n' \
    "$checked" "$legacy_debt" "$compare_base"
