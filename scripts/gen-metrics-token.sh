#!/usr/bin/env bash
# Generate the bearer token shared by the Aero /metrics endpoint and the
# Compose Prometheus scraper. Existing token material is never overwritten.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SECRETS_DIR="$REPO_ROOT/secrets"
REQUESTED_TOKEN_FILE="${AERO_METRICS_TOKEN_FILE:-$SECRETS_DIR/metrics_token}"
REQUESTED_TOKEN_DIR="$(dirname "$REQUESTED_TOKEN_FILE")"

umask 077
mkdir -p "$REQUESTED_TOKEN_DIR"
TOKEN_DIR="$(cd "$REQUESTED_TOKEN_DIR" && pwd)"
TOKEN_FILE="$TOKEN_DIR/$(basename "$REQUESTED_TOKEN_FILE")"

if [[ "$TOKEN_DIR" == "$SECRETS_DIR" ]]; then
    chmod 700 "$TOKEN_DIR"
elif [[ -n "$(find "$TOKEN_DIR" -maxdepth 0 -perm /077 -print -quit)" ]]; then
    echo "gen-metrics-token: custom token directory must deny group/other access: $TOKEN_DIR" >&2
    exit 1
fi

if [[ -e "$TOKEN_FILE" ]]; then
    if [[ ! -f "$TOKEN_FILE" || ! -s "$TOKEN_FILE" ]]; then
        echo "gen-metrics-token: existing token path is not a non-empty file: $TOKEN_FILE" >&2
        exit 1
    fi
    # The parent secrets directory is 0700. The file itself must be readable by
    # the unprivileged Prometheus container when Compose bind-mounts it.
    chmod 644 "$TOKEN_FILE"
    echo "Metrics token already present at $TOKEN_FILE (not overwritten)."
    exit 0
fi

if ! command -v openssl >/dev/null 2>&1; then
    echo "gen-metrics-token: openssl not found in PATH" >&2
    exit 1
fi

partial="${TOKEN_FILE}.partial.$$"
cleanup() {
    rm -f "$partial"
}
trap cleanup EXIT INT TERM
openssl rand -hex 32 >"$partial"
mv "$partial" "$TOKEN_FILE"
chmod 644 "$TOKEN_FILE"
trap - EXIT INT TERM
echo "Generated metrics bearer token at $TOKEN_FILE"
