#!/usr/bin/env bash
# Reject edits, renames, and deletions of migrations that already exist on the
# selected Git baseline. New, monotonically numbered migration files are the
# only supported schema-change path.

set -euo pipefail

REPO_ROOT="${AERO_MIGRATION_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
MIGRATION_DIR="${AERO_MIGRATION_DIR:-migrations}"

cd "$REPO_ROOT"

if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    echo "migration-immutability: $REPO_ROOT is not a Git worktree" >&2
    exit 1
fi

if [[ ! -d "$MIGRATION_DIR" ]]; then
    echo "migration-immutability: missing $MIGRATION_DIR/" >&2
    exit 1
fi

mapfile -t migration_names < <(
    find "$MIGRATION_DIR" -maxdepth 1 -type f -name '*.sql' -printf '%f\n' | sort
)

if (( ${#migration_names[@]} == 0 )); then
    echo "migration-immutability: no SQL migrations found" >&2
    exit 1
fi

declare -A seen_prefixes=()
for name in "${migration_names[@]}"; do
    if [[ ! "$name" =~ ^([0-9]{4})_[a-z0-9_]+\.sql$ ]]; then
        echo "migration-immutability: invalid filename: $MIGRATION_DIR/$name" >&2
        echo "migration-immutability: expected NNNN_lower_snake_case.sql" >&2
        exit 1
    fi

    prefix="${BASH_REMATCH[1]}"
    if [[ -n "${seen_prefixes[$prefix]:-}" ]]; then
        echo "migration-immutability: duplicate sequence $prefix:" >&2
        echo "  $MIGRATION_DIR/${seen_prefixes[$prefix]}" >&2
        echo "  $MIGRATION_DIR/$name" >&2
        exit 1
    fi
    seen_prefixes[$prefix]="$name"
done

requested_base="${AERO_MIGRATION_BASE_REF:-}"
if [[ -z "$requested_base" && -n "${GITHUB_BASE_REF:-}" ]]; then
    requested_base="origin/${GITHUB_BASE_REF}"
fi
if [[ -z "$requested_base" ]]; then
    requested_base="HEAD"
fi

# GitHub uses an all-zero `before` SHA for the first push of a branch. It is
# not a usable baseline, so compare with the first parent when one exists.
if [[ "$requested_base" =~ ^0+$ ]]; then
    if git rev-parse --verify --quiet HEAD^ >/dev/null; then
        requested_base="HEAD^"
    else
        requested_base="HEAD"
    fi
fi

if ! git rev-parse --verify --quiet "${requested_base}^{commit}" >/dev/null; then
    echo "migration-immutability: unknown baseline: $requested_base" >&2
    exit 1
fi

compare_base="$requested_base"
if [[ "$requested_base" != "HEAD" ]]; then
    if ! compare_base="$(git merge-base "$requested_base" HEAD)"; then
        echo "migration-immutability: cannot find merge-base for $requested_base" >&2
        exit 1
    fi
fi

baseline_max=-1
while IFS= read -r path; do
    name="${path##*/}"
    if [[ "$name" =~ ^([0-9]{4})_.*\.sql$ ]]; then
        sequence=$((10#${BASH_REMATCH[1]}))
        (( sequence > baseline_max )) && baseline_max=$sequence
    fi
done < <(git ls-tree -r --name-only "$compare_base" -- "$MIGRATION_DIR")

if (( baseline_max >= 0 )); then
    for name in "${migration_names[@]}"; do
        path="$MIGRATION_DIR/$name"
        if git cat-file -e "$compare_base:$path" 2>/dev/null; then
            continue
        fi
        # The filename has already passed the stricter validation above.
        sequence=$((10#${name%%_*}))
        if (( sequence <= baseline_max )); then
            printf 'migration-immutability: new migration %s must follow baseline sequence %04d\n' \
                "$path" "$baseline_max" >&2
            exit 1
        fi
    done
fi

violations=()
while IFS=$'\t' read -r status old_path new_path; do
    [[ -z "$status" ]] && continue
    if [[ "$status" == A* ]]; then
        continue
    fi

    if [[ "$status" == R* || "$status" == C* ]]; then
        violations+=("$status $old_path -> $new_path")
    else
        violations+=("$status $old_path")
    fi
done < <(git diff --name-status --find-renames "$compare_base" -- "$MIGRATION_DIR")

if (( ${#violations[@]} > 0 )); then
    echo "migration-immutability: historical migrations changed:" >&2
    printf '  %s\n' "${violations[@]}" >&2
    echo "migration-immutability: restore them and add the next migration instead" >&2
    exit 1
fi

printf 'migration-immutability: PASS (%d migrations; baseline %s)\n' \
    "${#migration_names[@]}" "$compare_base"
