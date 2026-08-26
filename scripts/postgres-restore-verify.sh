#!/usr/bin/env bash
# Verify a PostgreSQL custom-format backup by restoring it into a newly created,
# disposable database. Existing databases are never dropped or overwritten.

set -euo pipefail

CONTAINER="${AERO_BACKUP_POSTGRES_CONTAINER:-aero-postgres}"
SOURCE_DATABASE="${AERO_BACKUP_DATABASE:-aero}"
DB_USER="${AERO_BACKUP_USER:-aero}"
TARGET_DATABASE="${AERO_RESTORE_VERIFY_DATABASE:-aero_restore_verify_$(date +%s)_$$}"
KEEP_DATABASE="${AERO_RESTORE_KEEP_DATABASE:-0}"
ARCHIVE="${1:-}"
CREATED=0

cleanup() {
    status=$?
    trap - EXIT INT TERM
    if (( CREATED == 1 )) && [[ "$KEEP_DATABASE" != 1 ]]; then
        docker exec "$CONTAINER" dropdb \
            --username "$DB_USER" --force --if-exists "$TARGET_DATABASE" \
            >/dev/null 2>&1 || true
    fi
    exit "$status"
}
trap cleanup EXIT INT TERM

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "postgres-restore-verify: missing required command: $1" >&2
        exit 1
    fi
}

require_command docker
require_command sha256sum

if [[ -z "$ARCHIVE" || ! -f "$ARCHIVE" ]]; then
    echo "usage: scripts/postgres-restore-verify.sh PATH_TO_DUMP" >&2
    exit 2
fi
if [[ ! "$TARGET_DATABASE" =~ ^aero_restore_verify_[a-zA-Z0-9_]+$ ]]; then
    echo "postgres-restore-verify: target must start with aero_restore_verify_: $TARGET_DATABASE" >&2
    exit 2
fi
if [[ "$TARGET_DATABASE" == "$SOURCE_DATABASE" ]]; then
    echo "postgres-restore-verify: target must differ from source database" >&2
    exit 2
fi
if [[ "$(docker inspect --format '{{.State.Running}}' "$CONTAINER" 2>/dev/null || true)" != true ]]; then
    echo "postgres-restore-verify: container is not running: $CONTAINER" >&2
    exit 1
fi

if [[ -f "${ARCHIVE}.sha256" ]]; then
    archive_dir="$(cd "$(dirname "$ARCHIVE")" && pwd)"
    archive_name="$(basename "$ARCHIVE")"
    (
        cd "$archive_dir"
        sha256sum --check --status "${archive_name}.sha256"
    )
    echo "postgres-restore-verify: checksum PASS"
else
    echo "postgres-restore-verify: WARN no checksum sidecar found" >&2
fi

docker exec --interactive "$CONTAINER" pg_restore --list <"$ARCHIVE" >/dev/null

# createdb fails if the target already exists. CREATED is set only afterward,
# so cleanup can never delete a database that this invocation did not create.
docker exec "$CONTAINER" createdb \
    --username "$DB_USER" \
    --owner "$DB_USER" \
    --template template0 \
    --encoding UTF8 \
    "$TARGET_DATABASE"
CREATED=1

echo "postgres-restore-verify: restoring into $TARGET_DATABASE"
docker exec --interactive "$CONTAINER" pg_restore \
    --username "$DB_USER" \
    --dbname "$TARGET_DATABASE" \
    --exit-on-error \
    --single-transaction \
    --no-owner \
    --no-privileges \
    <"$ARCHIVE"

missing_relations="$(
    docker exec "$CONTAINER" psql \
        --username "$DB_USER" --dbname "$TARGET_DATABASE" \
        --no-align --tuples-only --set ON_ERROR_STOP=1 \
        --command "
            SELECT COALESCE(string_agg(name, ',' ORDER BY name), '')
            FROM (VALUES
                ('_sqlx_migrations'),
                ('participants'),
                ('rooms'),
                ('messages')
            ) AS required(name)
            WHERE to_regclass('public.' || name) IS NULL;
        "
)"
if [[ -n "$missing_relations" ]]; then
    echo "postgres-restore-verify: missing required relations: $missing_relations" >&2
    exit 1
fi

read -r successful_migrations failed_migrations <<<"$(
    docker exec "$CONTAINER" psql \
        --username "$DB_USER" --dbname "$TARGET_DATABASE" \
        --no-align --tuples-only --field-separator ' ' --set ON_ERROR_STOP=1 \
        --command "
            SELECT count(*) FILTER (WHERE success), count(*) FILTER (WHERE NOT success)
            FROM _sqlx_migrations;
        "
)"
if [[ "$successful_migrations" -le 0 || "$failed_migrations" -ne 0 ]]; then
    echo "postgres-restore-verify: invalid migration ledger: successful=$successful_migrations failed=$failed_migrations" >&2
    exit 1
fi

printf 'postgres-restore-verify: PASS (%s successful migrations, target=%s)\n' \
    "$successful_migrations" "$TARGET_DATABASE"
if [[ "$KEEP_DATABASE" == 1 ]]; then
    echo "postgres-restore-verify: keeping target database by request"
fi
