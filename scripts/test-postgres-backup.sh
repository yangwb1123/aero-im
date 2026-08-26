#!/usr/bin/env bash
# Hermetic regression test for postgres-backup.sh publication semantics.
# It replaces Docker with a small stub and races two writers at the same
# destination; no PostgreSQL container or image is required.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEMP_DIR="$(mktemp -d -t aero-backup-test.XXXXXX)"
FAKE_BIN="$TEMP_DIR/bin"
mkdir -p "$FAKE_BIN"
trap 'rm -rf "$TEMP_DIR"' EXIT INT TERM

cat >"$FAKE_BIN/docker" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == inspect ]]; then
    if [[ "${AERO_BACKUP_TEST_BARRIER:-0}" == 1 ]]; then
        marker="$AERO_BACKUP_TEST_DIR/inspect.$PPID"
        : >"$marker"
        for _ in {1..200}; do
            count="$(find "$AERO_BACKUP_TEST_DIR" -maxdepth 1 -name 'inspect.*' -type f | wc -l)"
            (( count >= 2 )) && break
            sleep 0.01
        done
    fi
    printf 'true\n'
    exit 0
fi

if [[ "${1:-}" == exec ]]; then
    if [[ " $* " == *" pg_dump "* ]]; then
        printf 'custom archive from publisher %s\n' "$PPID"
        exit 0
    fi
    if [[ " $* " == *" pg_restore "* ]]; then
        exit 0
    fi
fi

echo "unexpected fake docker invocation: $*" >&2
exit 1
STUB
chmod 0755 "$FAKE_BIN/docker"

cat >"$FAKE_BIN/sha256sum" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${AERO_BACKUP_TEST_RACE_CHECKSUM:-0}" == 1 ]]; then
    : >"$AERO_BACKUP_TEST_SIDE_CAR"
fi
exec /usr/bin/sha256sum "$@"
STUB
chmod 0755 "$FAKE_BIN/sha256sum"

OUTPUT="$TEMP_DIR/aero.dump"
export PATH="$FAKE_BIN:$PATH"
export AERO_BACKUP_POSTGRES_CONTAINER=aero-test-postgres
export AERO_BACKUP_DATABASE=aero
export AERO_BACKUP_USER=aero
export AERO_BACKUP_TEST_DIR="$TEMP_DIR"
export AERO_BACKUP_TEST_BARRIER=1

set +e
bash "$ROOT/scripts/postgres-backup.sh" "$OUTPUT" >"$TEMP_DIR/first.log" 2>&1 &
first_pid=$!
bash "$ROOT/scripts/postgres-backup.sh" "$OUTPUT" >"$TEMP_DIR/second.log" 2>&1 &
second_pid=$!
wait "$first_pid"
first_status=$?
wait "$second_pid"
second_status=$?
set -e

if ! { [[ "$first_status" -eq 0 && "$second_status" -ne 0 ]] ||
       [[ "$second_status" -eq 0 && "$first_status" -ne 0 ]]; }; then
    echo "backup-test: expected exactly one concurrent publisher to succeed (statuses $first_status/$second_status)" >&2
    cat "$TEMP_DIR/first.log" "$TEMP_DIR/second.log" >&2
    exit 1
fi

if [[ ! -s "$OUTPUT" || ! -s "${OUTPUT}.sha256" ]]; then
    echo "backup-test: winning publication is incomplete" >&2
    exit 1
fi
expected_checksum="$(sha256sum "$OUTPUT" | awk '{print $1}')"
actual_checksum="$(awk '{print $1}' "${OUTPUT}.sha256")"
if [[ "$actual_checksum" != "$expected_checksum" ]]; then
    echo "backup-test: checksum sidecar does not match the archive" >&2
    exit 1
fi
if [[ "$(stat -c '%a' "$OUTPUT")" != 600 || "$(stat -c '%a' "${OUTPUT}.sha256")" != 600 ]]; then
    echo "backup-test: published files do not retain mode 600" >&2
    exit 1
fi
if compgen -G "${OUTPUT}.partial.*" >/dev/null || compgen -G "${OUTPUT}.sha256.partial.*" >/dev/null; then
    echo "backup-test: temporary publication files were not cleaned up" >&2
    exit 1
fi
if ! grep -Fq 'refusing to overwrite existing backup' "$TEMP_DIR/first.log" "$TEMP_DIR/second.log"; then
    echo "backup-test: losing publisher did not preserve the existing refusal message" >&2
    exit 1
fi

archive_checksum_before="$expected_checksum"
export AERO_BACKUP_TEST_BARRIER=0
set +e
bash "$ROOT/scripts/postgres-backup.sh" "$OUTPUT" >"$TEMP_DIR/existing.log" 2>&1
existing_status=$?
set -e
if [[ "$existing_status" -eq 0 ]] ||
   ! grep -Fq "postgres-backup: refusing to overwrite existing backup: $OUTPUT" "$TEMP_DIR/existing.log"; then
    echo "backup-test: existing destination was not refused with the established error" >&2
    cat "$TEMP_DIR/existing.log" >&2
    exit 1
fi
if [[ "$(sha256sum "$OUTPUT" | awk '{print $1}')" != "$archive_checksum_before" ]]; then
    echo "backup-test: refusal changed the existing archive" >&2
    exit 1
fi

# Force the checksum destination to appear after preflight but before its
# publication. The archive link must be rolled back rather than left alone.
CHECKSUM_RACE_OUTPUT="$TEMP_DIR/checksum-race.dump"
export AERO_BACKUP_TEST_RACE_CHECKSUM=1
export AERO_BACKUP_TEST_SIDE_CAR="${CHECKSUM_RACE_OUTPUT}.sha256"
set +e
bash "$ROOT/scripts/postgres-backup.sh" "$CHECKSUM_RACE_OUTPUT" >"$TEMP_DIR/checksum-race.log" 2>&1
checksum_race_status=$?
set -e
export AERO_BACKUP_TEST_RACE_CHECKSUM=0
if [[ "$checksum_race_status" -eq 0 || -e "$CHECKSUM_RACE_OUTPUT" ]]; then
    echo "backup-test: partial checksum publication left an archive behind" >&2
    cat "$TEMP_DIR/checksum-race.log" >&2
    exit 1
fi
if [[ ! -e "${CHECKSUM_RACE_OUTPUT}.sha256" ]] ||
   compgen -G "${CHECKSUM_RACE_OUTPUT}.partial.*" >/dev/null ||
   compgen -G "${CHECKSUM_RACE_OUTPUT}.sha256.partial.*" >/dev/null; then
    echo "backup-test: partial checksum publication cleanup failed" >&2
    exit 1
fi

echo "backup-test: PASS"
