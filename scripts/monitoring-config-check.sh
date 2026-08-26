#!/usr/bin/env bash
# Validate Prometheus/Alertmanager configuration and Grafana JSON with the same
# digest-pinned tool images used by Compose.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROMETHEUS_IMAGE="${AERO_PROMETHEUS_IMAGE:-quay.io/prometheus/prometheus:v3.13.1@sha256:3c42b892cf723fa54d2f262c37a0e1f80aa8c8ddb1da7b9b0df9455a35a7f893}"
ALERTMANAGER_IMAGE="${AERO_ALERTMANAGER_IMAGE:-quay.io/prometheus/alertmanager:v0.33.1@sha256:9e082985f56f4c8c9f724e18f2288c6708f472e56a5286b8863d080434ea065d}"
TEMP_DIR="$(mktemp -d -t aero-monitoring-check.XXXXXX)"

cleanup() {
    rm -rf "$TEMP_DIR"
}
trap cleanup EXIT INT TERM

for command in docker jq sha256sum stat; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "monitoring-check: missing required command: $command" >&2
        exit 1
    fi
done

AERO_METRICS_TOKEN_FILE="$TEMP_DIR/metrics_token" \
    bash "$REPO_ROOT/scripts/gen-metrics-token.sh" >/dev/null
token_checksum="$(sha256sum "$TEMP_DIR/metrics_token" | awk '{print $1}')"
AERO_METRICS_TOKEN_FILE="$TEMP_DIR/metrics_token" \
    bash "$REPO_ROOT/scripts/gen-metrics-token.sh" >/dev/null
if [[ "$(sha256sum "$TEMP_DIR/metrics_token" | awk '{print $1}')" != "$token_checksum" ]]; then
    echo "monitoring-check: metrics token generator overwrote an existing token" >&2
    exit 1
fi
if [[ "$(stat -c '%a' "$TEMP_DIR/metrics_token")" != 644 ]] || \
   ! grep -Eq '^[0-9a-f]{64}$' "$TEMP_DIR/metrics_token"; then
    echo "monitoring-check: generated metrics token has invalid mode or format" >&2
    exit 1
fi

docker run --rm \
    --entrypoint /bin/promtool \
    --mount "type=bind,source=$REPO_ROOT/monitoring/prometheus,target=/etc/prometheus,readonly" \
    --mount "type=bind,source=$TEMP_DIR/metrics_token,target=/run/secrets/aero_metrics_token,readonly" \
    "$PROMETHEUS_IMAGE" \
    check config /etc/prometheus/prometheus.yml

docker run --rm \
    --entrypoint /bin/amtool \
    --mount "type=bind,source=$REPO_ROOT/monitoring/alertmanager.example.yml,target=/etc/alertmanager/alertmanager.yml,readonly" \
    "$ALERTMANAGER_IMAGE" \
    check-config /etc/alertmanager/alertmanager.yml

jq --exit-status 'type == "object" and .uid == "aero-red" and (.panels | length > 0)' \
    "$REPO_ROOT/monitoring/grafana/aero-red-dashboard.json" >/dev/null

echo "monitoring-check: PASS"
