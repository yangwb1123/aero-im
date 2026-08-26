#!/usr/bin/env bash
# Create an atomic PostgreSQL custom-format backup through the Compose database
# container. The dump contains application data and must be treated as sensitive.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTAINER="${AERO_BACKUP_POSTGRES_CONTAINER:-aero-postgres}"
DATABASE="${AERO_BACKUP_DATABASE:-aero}"
DB_USER="${AERO_BACKUP_USER:-aero}"
TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUTPUT="${1:-${AERO_POSTGRES_BACKUP_OUTPUT:-$REPO_ROOT/data/backups/postgres/${DATABASE}-${TIMESTAMP}.dump}}"
PARTIAL="${OUTPUT}.partial.$$"
CHECKSUM_PARTIAL="${OUTPUT}.sha256.partial.$$"
ARCHIVE_PUBLISHED=0
CHECKSUM_PUBLISHED=0

cleanup() {
    # If publication stopped between the two no-clobber links, remove only the
    # archive inode this invocation created. The -ef check avoids deleting a
    # destination that another process replaced after our link.
    if (( ARCHIVE_PUBLISHED == 1 && CHECKSUM_PUBLISHED == 0 )) &&
        [[ -e "$PARTIAL" && -e "$OUTPUT" && "$PARTIAL" -ef "$OUTPUT" ]]; then
        rm -f -- "$OUTPUT" || true
    fi
    rm -f -- "$PARTIAL" "$CHECKSUM_PARTIAL" || true
}

# A hard link in the destination directory is an atomic, same-filesystem
# install. Unlike mv, ln -T fails if the destination appears after preflight;
# it never truncates or replaces an existing archive.
publish_noclobber() {
    local source="$1"
    local destination="$2"
    if ln -T -- "$source" "$destination" 2>/dev/null; then
        return 0
    fi
    if [[ -e "$destination" || -L "$destination" ]]; then
        return 2
    fi
    return 1
}

report_publish_failure() {
    local status="$1"
    if (( status == 2 )); then
        # Keep the preflight error wording for a destination that raced us.
        echo "postgres-backup: refusing to overwrite existing backup: $OUTPUT" >&2
    else
        echo "postgres-backup: failed to publish backup: $OUTPUT" >&2
    fi
}
trap cleanup EXIT INT TERM

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "postgres-backup: missing required command: $1" >&2
        exit 1
    fi
}

require_command docker
require_command sha256sum

if [[ -z "$DATABASE" || -z "$DB_USER" ]]; then
    echo "postgres-backup: database and user must be non-empty" >&2
    exit 1
fi
if [[ -e "$OUTPUT" || -e "${OUTPUT}.sha256" ]]; then
    echo "postgres-backup: refusing to overwrite existing backup: $OUTPUT" >&2
    exit 1
fi
if [[ "$(docker inspect --format '{{.State.Running}}' "$CONTAINER" 2>/dev/null || true)" != true ]]; then
    echo "postgres-backup: container is not running: $CONTAINER" >&2
    exit 1
fi

mkdir -p "$(dirname "$OUTPUT")"
umask 077

echo "postgres-backup: dumping $DATABASE from $CONTAINER"
docker exec "$CONTAINER" pg_dump \
    --username "$DB_USER" \
    --dbname "$DATABASE" \
    --format custom \
    --compress 9 \
    --no-owner \
    --no-privileges \
    --serializable-deferrable \
    --lock-wait-timeout "${AERO_BACKUP_LOCK_TIMEOUT:-10s}" \
    >"$PARTIAL"

if [[ ! -s "$PARTIAL" ]]; then
    echo "postgres-backup: pg_dump produced an empty file" >&2
    exit 1
fi

# Parse the archive with the matching pg_restore binary before publishing it.
docker exec --interactive "$CONTAINER" pg_restore --list <"$PARTIAL" >/dev/null

checksum="$(sha256sum "$PARTIAL" | awk '{print $1}')"
printf '%s  %s\n' "$checksum" "$(basename "$OUTPUT")" >"$CHECKSUM_PARTIAL"

# Set the final permissions before linking. The hard links below then publish
# files that are already private, even if a later publication step fails.
chmod 600 "$PARTIAL" "$CHECKSUM_PARTIAL"

if publish_noclobber "$PARTIAL" "$OUTPUT"; then
    ARCHIVE_PUBLISHED=1
else
    publish_status=$?
    report_publish_failure "$publish_status"
    exit 1
fi

if publish_noclobber "$CHECKSUM_PARTIAL" "${OUTPUT}.sha256"; then
    CHECKSUM_PUBLISHED=1
else
    publish_status=$?
    report_publish_failure "$publish_status"
    exit 1
fi

chmod 600 "$OUTPUT" "${OUTPUT}.sha256"
rm -f -- "$PARTIAL" "$CHECKSUM_PARTIAL"
trap - EXIT INT TERM

size_bytes="$(wc -c <"$OUTPUT" | tr -d ' ')"
printf 'postgres-backup: PASS (%s bytes)\n' "$size_bytes"
printf 'postgres-backup: archive=%s\n' "$OUTPUT"
printf 'postgres-backup: checksum=%s.sha256\n' "$OUTPUT"
