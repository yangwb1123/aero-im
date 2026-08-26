#!/usr/bin/env bash
# Static regression checks for the normal Docker deployment's listener and
# healthcheck wiring. Keep this hermetic: docker image pulls are not required.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE="$ROOT/docker-compose.yml"
DOCKERFILE="$ROOT/docker/Dockerfile"

assert_contains() {
    local path="$1"
    local text="$2"
    if ! grep -Fq -- "$text" "$path"; then
        echo "deployment-config: missing '$text' in $path" >&2
        exit 1
    fi
}

assert_contains "$COMPOSE" 'AERO__LIVE__SRT_LISTEN: 0.0.0.0:1936'
assert_contains "$COMPOSE" '1936:1936/udp'
assert_contains "$COMPOSE" '/usr/bin/wget -qO- http://localhost:16686/'
assert_contains "$DOCKERFILE" 'EXPOSE 8080/tcp 1935/tcp 1936/udp'

# The Jaeger healthcheck must not regress to the old /bin/wget path: the
# pinned Jaeger 2.20 image provides wget at /usr/bin/wget.
jaeger_healthcheck="$(grep -F 'http://localhost:16686/' "$COMPOSE" || true)"
if [[ "$jaeger_healthcheck" == *'"/bin/wget '* ]]; then
    echo "deployment-config: Jaeger healthcheck uses the unavailable /bin/wget" >&2
    exit 1
fi

echo "deployment-config: PASS"
