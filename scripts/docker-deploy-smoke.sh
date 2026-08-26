#!/usr/bin/env bash
# Build the production images and verify a fresh, isolated deployment without
# touching the developer Compose stack or its persistent data.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_TOKEN="$(date +%s)-$$"
NETWORK_NAME="aero-smoke-$RUN_TOKEN"
POSTGRES_NAME="aero-smoke-postgres-$RUN_TOKEN"
REDIS_NAME="aero-smoke-redis-$RUN_TOKEN"
NATS_NAME="aero-smoke-nats-$RUN_TOKEN"
MIGRATE_NAME="aero-smoke-migrate-$RUN_TOKEN"
APP_NAME="aero-smoke-app-$RUN_TOKEN"
PROMETHEUS_NAME="aero-smoke-prometheus-$RUN_TOKEN"
ALERTMANAGER_NAME="aero-smoke-alertmanager-$RUN_TOKEN"
GRAFANA_NAME="aero-smoke-grafana-$RUN_TOKEN"
RESTORE_DATABASE="aero_restore_verify_smoke_${RUN_TOKEN//-/_}"
PREEXISTING_RESTORE_DATABASE="aero_restore_verify_existing_${RUN_TOKEN//-/_}"
RUNTIME_IMAGE="${AERO_DOCKER_RUNTIME_IMAGE:-aero-im-runtime-smoke:$RUN_TOKEN}"
MIGRATE_IMAGE="${AERO_DOCKER_MIGRATE_IMAGE:-aero-im-migrate-smoke:$RUN_TOKEN}"
POSTGRES_IMAGE="${AERO_POSTGRES_IMAGE:-pgvector/pgvector:pg17@sha256:feb68f4f15446397d8cac7f4fe48fe4586de83160d1fc48b46283312d1a33966}"
REDIS_IMAGE="${AERO_REDIS_IMAGE:-redis:7-alpine@sha256:6ab0b6e7381779332f97b8ca76193e45b0756f38d4c0dcda72dbb3c32061ab99}"
NATS_IMAGE="${AERO_NATS_IMAGE:-nats:2.10-alpine@sha256:b83efabe3e7def1e0a4a31ec6e078999bb17c80363f881df35edc70fcb6bb927}"
PROMETHEUS_IMAGE="${AERO_PROMETHEUS_IMAGE:-quay.io/prometheus/prometheus:v3.13.1@sha256:3c42b892cf723fa54d2f262c37a0e1f80aa8c8ddb1da7b9b0df9455a35a7f893}"
ALERTMANAGER_IMAGE="${AERO_ALERTMANAGER_IMAGE:-quay.io/prometheus/alertmanager:v0.33.1@sha256:9e082985f56f4c8c9f724e18f2288c6708f472e56a5286b8863d080434ea065d}"
GRAFANA_IMAGE="${AERO_GRAFANA_IMAGE:-grafana/grafana:13.2.0@sha256:3fd54ae1214669f8355f065ec9f6445d5279a3d77095ab048ca045685272429b}"
TEMP_DIR="$(mktemp -d -t aero-docker-smoke.XXXXXX)"
BUILT_IMAGES=0

cleanup() {
    status=$?
    trap - EXIT INT TERM
    set +e

    if (( status != 0 )); then
        echo "docker-smoke: failure diagnostics" >&2
        docker logs --tail 200 "$APP_NAME" >&2 2>/dev/null
        docker logs --tail 200 "$POSTGRES_NAME" >&2 2>/dev/null
        docker logs --tail 200 "$REDIS_NAME" >&2 2>/dev/null
        docker logs --tail 200 "$NATS_NAME" >&2 2>/dev/null
        docker logs --tail 200 "$PROMETHEUS_NAME" >&2 2>/dev/null
        docker logs --tail 200 "$ALERTMANAGER_NAME" >&2 2>/dev/null
        docker logs --tail 200 "$GRAFANA_NAME" >&2 2>/dev/null
    fi

    docker rm --force --volumes \
        "$GRAFANA_NAME" "$PROMETHEUS_NAME" "$ALERTMANAGER_NAME" \
        "$APP_NAME" "$MIGRATE_NAME" "$NATS_NAME" "$REDIS_NAME" "$POSTGRES_NAME" \
        >/dev/null 2>&1
    docker network rm "$NETWORK_NAME" >/dev/null 2>&1
    if (( BUILT_IMAGES == 1 )); then
        docker image rm "$MIGRATE_IMAGE" "$RUNTIME_IMAGE" >/dev/null 2>&1
    fi
    rm -rf "$TEMP_DIR"
    exit "$status"
}
trap cleanup EXIT INT TERM

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "docker-smoke: missing required command: $1" >&2
        exit 1
    fi
}

wait_for_health() {
    container="$1"
    attempts="${2:-60}"
    for (( attempt = 1; attempt <= attempts; attempt++ )); do
        status="$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$container" 2>/dev/null || true)"
        case "$status" in
            healthy | running)
                return 0
                ;;
            unhealthy | exited | dead)
                echo "docker-smoke: $container entered $status" >&2
                return 1
                ;;
        esac
        sleep 2
    done
    echo "docker-smoke: timed out waiting for $container" >&2
    return 1
}

wait_for_url() {
    url="$1"
    label="$2"
    attempts="${3:-60}"
    for (( attempt = 1; attempt <= attempts; attempt++ )); do
        if docker exec "$APP_NAME" curl -fsS "$url" >/dev/null 2>&1; then
            return 0
        fi
        sleep 2
    done
    echo "docker-smoke: timed out waiting for $label at $url" >&2
    return 1
}

require_command docker
require_command openssl
require_command sha256sum

cd "$REPO_ROOT"
docker info >/dev/null

if [[ "${AERO_DOCKER_SKIP_BUILD:-0}" != "1" ]]; then
    BUILT_IMAGES=1
    echo "docker-smoke: building runtime image"
    docker build --file docker/Dockerfile --target runtime --tag "$RUNTIME_IMAGE" .
    echo "docker-smoke: building migration image"
    docker build --file docker/Dockerfile --target migrate --tag "$MIGRATE_IMAGE" .
else
    docker image inspect "$RUNTIME_IMAGE" "$MIGRATE_IMAGE" >/dev/null
fi

openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
    -out "$TEMP_DIR/jwt_private.pem" >/dev/null 2>&1
openssl pkey -in "$TEMP_DIR/jwt_private.pem" -traditional \
    -out "$TEMP_DIR/jwt_private.pkcs1.pem" >/dev/null 2>&1
mv "$TEMP_DIR/jwt_private.pkcs1.pem" "$TEMP_DIR/jwt_private.pem"
openssl rsa -in "$TEMP_DIR/jwt_private.pem" -pubout \
    -out "$TEMP_DIR/jwt_public.pem" >/dev/null 2>&1
openssl rand -hex 32 >"$TEMP_DIR/metrics_token"
chmod 600 "$TEMP_DIR/jwt_private.pem"
chmod 644 "$TEMP_DIR/jwt_public.pem"
chmod 644 "$TEMP_DIR/metrics_token"

docker network create "$NETWORK_NAME" >/dev/null

docker run --detach --name "$POSTGRES_NAME" --network "$NETWORK_NAME" \
    --network-alias postgres \
    --env POSTGRES_DB=aero \
    --env POSTGRES_USER=aero \
    --env POSTGRES_PASSWORD=aero_smoke_pw \
    --health-cmd 'pg_isready -U aero -d aero' \
    --health-interval 2s --health-timeout 2s --health-retries 30 \
    "$POSTGRES_IMAGE" >/dev/null

docker run --detach --name "$REDIS_NAME" --network "$NETWORK_NAME" \
    --network-alias redis \
    --health-cmd 'redis-cli ping' \
    --health-interval 2s --health-timeout 2s --health-retries 30 \
    "$REDIS_IMAGE" redis-server --save '' >/dev/null

docker run --detach --name "$NATS_NAME" --network "$NETWORK_NAME" \
    --network-alias nats \
    --health-cmd 'wget -qO- http://localhost:8222/healthz || exit 1' \
    --health-interval 2s --health-timeout 2s --health-retries 30 \
    "$NATS_IMAGE" -js -m 8222 >/dev/null

wait_for_health "$POSTGRES_NAME"
wait_for_health "$REDIS_NAME"
wait_for_health "$NATS_NAME"

echo "docker-smoke: applying embedded migrations to fresh PostgreSQL"
docker run --rm --name "$MIGRATE_NAME" --network "$NETWORK_NAME" \
    --env AERO__DATABASE__URL=postgres://aero:aero_smoke_pw@postgres:5432/aero \
    --env AERO__REDIS__URL=redis://redis:6379 \
    --env AERO__NATS__URL=nats://nats:4222 \
    "$MIGRATE_IMAGE"

expected_migrations="$(find migrations -maxdepth 1 -type f -name '*.sql' | wc -l)"
applied_migrations="$(
    docker exec --env PGPASSWORD=aero_smoke_pw "$POSTGRES_NAME" \
        psql -U aero -d aero -Atc \
        'SELECT count(*) FROM _sqlx_migrations WHERE success'
)"
if [[ "$applied_migrations" -ne "$expected_migrations" ]]; then
    echo "docker-smoke: migration count mismatch: applied=$applied_migrations expected=$expected_migrations" >&2
    exit 1
fi

echo "docker-smoke: exercising atomic backup and disposable restore"
docker exec --env PGPASSWORD=aero_smoke_pw "$POSTGRES_NAME" \
    psql -U aero -d aero -v ON_ERROR_STOP=1 --command "
        CREATE TABLE public.aero_backup_restore_smoke_marker (
            marker_id integer PRIMARY KEY,
            payload text NOT NULL
        );
        INSERT INTO public.aero_backup_restore_smoke_marker(marker_id, payload)
        VALUES (1, 'alpha'), (2, 'beta'), (3, 'gamma');
    " >/dev/null

source_marker_fingerprint="$(
    docker exec --env PGPASSWORD=aero_smoke_pw "$POSTGRES_NAME" \
        psql -U aero -d aero -Atc \
        "SELECT md5(string_agg(marker_id::text || ':' || payload, ',' ORDER BY marker_id))
         FROM public.aero_backup_restore_smoke_marker;"
)"
source_ledger_fingerprint="$(
    docker exec --env PGPASSWORD=aero_smoke_pw "$POSTGRES_NAME" \
        psql -U aero -d aero -Atc \
        "SELECT md5(string_agg(version::text || ':' || encode(checksum, 'hex'), ',' ORDER BY version))
         FROM _sqlx_migrations WHERE success;"
)"

AERO_BACKUP_POSTGRES_CONTAINER="$POSTGRES_NAME" \
    AERO_BACKUP_DATABASE=aero \
    AERO_BACKUP_USER=aero \
    bash scripts/postgres-backup.sh "$TEMP_DIR/aero-smoke.dump"

archive_checksum_before="$(sha256sum "$TEMP_DIR/aero-smoke.dump" | awk '{print $1}')"
if AERO_BACKUP_POSTGRES_CONTAINER="$POSTGRES_NAME" \
    AERO_BACKUP_DATABASE=aero \
    AERO_BACKUP_USER=aero \
    bash scripts/postgres-backup.sh "$TEMP_DIR/aero-smoke.dump"; then
    echo "docker-smoke: backup unexpectedly overwrote an existing archive" >&2
    exit 1
fi
archive_checksum_after="$(sha256sum "$TEMP_DIR/aero-smoke.dump" | awk '{print $1}')"
if [[ "$archive_checksum_after" != "$archive_checksum_before" ]]; then
    echo "docker-smoke: refused backup changed the existing archive" >&2
    exit 1
fi

# The marker exists only to prove user data survives the archive. Remove it
# from the source before the gateway boots so the smoke schema remains canonical.
docker exec --env PGPASSWORD=aero_smoke_pw "$POSTGRES_NAME" \
    psql -U aero -d aero -v ON_ERROR_STOP=1 \
    --command 'DROP TABLE public.aero_backup_restore_smoke_marker' >/dev/null

AERO_BACKUP_POSTGRES_CONTAINER="$POSTGRES_NAME" \
    AERO_BACKUP_DATABASE=aero \
    AERO_BACKUP_USER=aero \
    AERO_RESTORE_VERIFY_DATABASE="$RESTORE_DATABASE" \
    AERO_RESTORE_KEEP_DATABASE=1 \
    bash scripts/postgres-restore-verify.sh "$TEMP_DIR/aero-smoke.dump"

restored_marker_fingerprint="$(
    docker exec --env PGPASSWORD=aero_smoke_pw "$POSTGRES_NAME" \
        psql -U aero -d "$RESTORE_DATABASE" -Atc \
        "SELECT md5(string_agg(marker_id::text || ':' || payload, ',' ORDER BY marker_id))
         FROM public.aero_backup_restore_smoke_marker;"
)"
restored_ledger_fingerprint="$(
    docker exec --env PGPASSWORD=aero_smoke_pw "$POSTGRES_NAME" \
        psql -U aero -d "$RESTORE_DATABASE" -Atc \
        "SELECT md5(string_agg(version::text || ':' || encode(checksum, 'hex'), ',' ORDER BY version))
         FROM _sqlx_migrations WHERE success;"
)"
if [[ "$restored_marker_fingerprint" != "$source_marker_fingerprint" ]]; then
    echo "docker-smoke: restored marker data fingerprint mismatch" >&2
    exit 1
fi
if [[ "$restored_ledger_fingerprint" != "$source_ledger_fingerprint" ]]; then
    echo "docker-smoke: restored migration ledger fingerprint mismatch" >&2
    exit 1
fi
docker exec "$POSTGRES_NAME" dropdb \
    --username aero --force "$RESTORE_DATABASE"

docker exec "$POSTGRES_NAME" createdb \
    --username aero --owner aero --template template0 \
    "$PREEXISTING_RESTORE_DATABASE"
preexisting_restore_oid="$(
    docker exec "$POSTGRES_NAME" psql -U aero -d postgres -Atc \
        "SELECT oid FROM pg_database WHERE datname = '$PREEXISTING_RESTORE_DATABASE';"
)"
if AERO_BACKUP_POSTGRES_CONTAINER="$POSTGRES_NAME" \
    AERO_BACKUP_DATABASE=aero \
    AERO_BACKUP_USER=aero \
    AERO_RESTORE_VERIFY_DATABASE="$PREEXISTING_RESTORE_DATABASE" \
    bash scripts/postgres-restore-verify.sh "$TEMP_DIR/aero-smoke.dump"; then
    echo "docker-smoke: restore unexpectedly accepted an existing target" >&2
    exit 1
fi
preexisting_restore_oid_after="$(
    docker exec "$POSTGRES_NAME" psql -U aero -d postgres -Atc \
        "SELECT oid FROM pg_database WHERE datname = '$PREEXISTING_RESTORE_DATABASE';"
)"
if [[ -z "$preexisting_restore_oid" || "$preexisting_restore_oid_after" != "$preexisting_restore_oid" ]]; then
    echo "docker-smoke: refused restore changed or deleted the existing target" >&2
    exit 1
fi
docker exec "$POSTGRES_NAME" dropdb \
    --username aero --force "$PREEXISTING_RESTORE_DATABASE"
echo "docker-smoke: backup/restore fingerprint PASS"

echo "docker-smoke: starting unprivileged gateway with file-backed JWT secrets"
docker run --detach --name "$APP_NAME" --network "$NETWORK_NAME" --network-alias app \
    --env AERO__DATABASE__URL=postgres://aero:aero_smoke_pw@postgres:5432/aero \
    --env AERO__REDIS__URL=redis://redis:6379 \
    --env AERO__NATS__URL=nats://nats:4222 \
    --env AERO__AUTH__JWT_PRIVATE_KEY_FILE=/run/secrets/aero_jwt_private_key \
    --env AERO__AUTH__JWT_PUBLIC_KEY_FILE=/run/secrets/aero_jwt_public_key \
    --env AERO_METRICS_TOKEN_FILE=/run/secrets/aero_metrics_token \
    --env AERO__AUTH__ISSUER=aero-docker-smoke \
    --env AERO_PUBLIC_BASE_URL=http://127.0.0.1:8080 \
    --mount "type=bind,source=$TEMP_DIR/jwt_private.pem,target=/run/secrets/aero_jwt_private_key,readonly" \
    --mount "type=bind,source=$TEMP_DIR/jwt_public.pem,target=/run/secrets/aero_jwt_public_key,readonly" \
    --mount "type=bind,source=$TEMP_DIR/metrics_token,target=/run/secrets/aero_metrics_token,readonly" \
    --health-cmd 'curl -fsS http://localhost:8080/health/live >/dev/null || exit 1' \
    --health-interval 2s --health-timeout 2s --health-retries 60 \
    "$RUNTIME_IMAGE" >/dev/null

wait_for_health "$APP_NAME" 75
liveness="$(docker exec "$APP_NAME" curl -fsS http://localhost:8080/health/live)"
readiness="$(docker exec "$APP_NAME" curl -fsS http://localhost:8080/health/ready)"
printf 'docker-smoke: liveness %s\n' "$liveness"
printf 'docker-smoke: readiness %s\n' "$readiness"

metrics_without_token_status="$(
    docker exec "$APP_NAME" curl --silent --output /dev/null \
        --write-out '%{http_code}' http://localhost:8080/metrics
)"
if [[ "$metrics_without_token_status" != 401 ]]; then
    echo "docker-smoke: bearer-gated metrics returned HTTP $metrics_without_token_status without a token" >&2
    exit 1
fi

echo "docker-smoke: starting Prometheus, Alertmanager, and provisioned Grafana"
docker run --detach --name "$ALERTMANAGER_NAME" --network "$NETWORK_NAME" \
    --network-alias alertmanager \
    --mount "type=bind,source=$REPO_ROOT/monitoring/alertmanager.example.yml,target=/etc/alertmanager/alertmanager.yml,readonly" \
    "$ALERTMANAGER_IMAGE" \
    --config.file=/etc/alertmanager/alertmanager.yml \
    --storage.path=/alertmanager >/dev/null

docker run --detach --name "$PROMETHEUS_NAME" --network "$NETWORK_NAME" \
    --network-alias prometheus \
    --mount "type=bind,source=$REPO_ROOT/monitoring/prometheus/prometheus.yml,target=/etc/prometheus/prometheus.yml,readonly" \
    --mount "type=bind,source=$REPO_ROOT/monitoring/prometheus/recording_rules.yml,target=/etc/prometheus/recording_rules.yml,readonly" \
    --mount "type=bind,source=$REPO_ROOT/monitoring/prometheus/alert_rules.yml,target=/etc/prometheus/alert_rules.yml,readonly" \
    --mount "type=bind,source=$TEMP_DIR/metrics_token,target=/run/secrets/aero_metrics_token,readonly" \
    "$PROMETHEUS_IMAGE" \
    --config.file=/etc/prometheus/prometheus.yml \
    --storage.tsdb.path=/prometheus >/dev/null

docker run --detach --name "$GRAFANA_NAME" --network "$NETWORK_NAME" \
    --network-alias grafana \
    --env GF_SECURITY_ADMIN_USER=admin \
    --env GF_SECURITY_ADMIN_PASSWORD=aero_grafana_smoke_pw \
    --env GF_USERS_ALLOW_SIGN_UP=false \
    --env GF_AUTH_ANONYMOUS_ENABLED=false \
    --env GF_PLUGINS_PREINSTALL_DISABLED=true \
    --mount "type=bind,source=$REPO_ROOT/monitoring/grafana/provisioning,target=/etc/grafana/provisioning,readonly" \
    --mount "type=bind,source=$REPO_ROOT/monitoring/grafana/aero-red-dashboard.json,target=/var/lib/grafana/dashboards/aero-red-dashboard.json,readonly" \
    "$GRAFANA_IMAGE" >/dev/null

wait_for_url http://alertmanager:9093/-/healthy Alertmanager
wait_for_url http://prometheus:9090/-/ready Prometheus
wait_for_url http://grafana:3000/api/health Grafana 90

prometheus_target_json=""
for (( attempt = 1; attempt <= 30; attempt++ )); do
    prometheus_target_json="$(
        docker exec "$APP_NAME" curl -fsS http://prometheus:9090/api/v1/targets
    )"
    if [[ "$prometheus_target_json" == *'"health":"up"'* ]] && \
       [[ "$prometheus_target_json" == *'http://app:8080/metrics'* ]]; then
        break
    fi
    sleep 2
done
if [[ "$prometheus_target_json" != *'"health":"up"'* ]] || \
   [[ "$prometheus_target_json" != *'http://app:8080/metrics'* ]]; then
    echo "docker-smoke: Prometheus did not scrape the bearer-gated Aero target" >&2
    exit 1
fi

prometheus_rules_json="$(
    docker exec "$APP_NAME" curl -fsS http://prometheus:9090/api/v1/rules
)"
if [[ "$prometheus_rules_json" != *'AeroHttpErrorBudgetFastBurn'* ]] || \
   [[ "$prometheus_rules_json" != *'AeroAuditOutboxSamplerDown'* ]] || \
   [[ "$prometheus_rules_json" != *'job:aero_http_requests:rate5m'* ]]; then
    echo "docker-smoke: Prometheus did not load the expected recording/alert rules" >&2
    exit 1
fi

grafana_datasource_json="$(
    docker exec "$APP_NAME" curl -fsS \
        --user admin:aero_grafana_smoke_pw \
        http://grafana:3000/api/datasources/uid/aero-prometheus
)"
grafana_dashboard_json="$(
    docker exec "$APP_NAME" curl -fsS \
        --user admin:aero_grafana_smoke_pw \
        'http://grafana:3000/api/search?query=Aero%20IM'
)"
if [[ "$grafana_datasource_json" != *'"uid":"aero-prometheus"'* ]] || \
   [[ "$grafana_dashboard_json" != *'"uid":"aero-red"'* ]]; then
    echo "docker-smoke: Grafana provisioning did not load the Aero datasource/dashboard" >&2
    exit 1
fi
echo "docker-smoke: observability scrape/rules/dashboard PASS"

probe_output="$(
    docker run --rm --entrypoint /usr/local/bin/aero-audit-relay-probe "$RUNTIME_IMAGE"
)"
printf '%s\n' "$probe_output"
probe_passes="$(printf '%s\n' "$probe_output" | awk '/^probe: .*: PASS$/ { count++ } END { print count + 0 }')"
if [[ "$probe_passes" -ne 9 ]] || grep -q '^probe: .*: FAIL' <<<"$probe_output"; then
    echo "docker-smoke: relay probe contract failed" >&2
    exit 1
fi

echo "docker-smoke: PASS ($applied_migrations migrations, backup/restore, observability, healthy gateway, relay probe 9/9)"
