# Aero IM monitoring

In-repo Prometheus / Grafana / Alertmanager config for the platform's RED signals
and error-budget alerting. Every rule here is **grounded in metrics the server
actually emits** (`crates/aero-common/src/metrics.rs`,
`crates/aero-server/src/metrics.rs`). Compose wiring, authenticated scraping,
rule loading and Grafana provisioning are executable in this repository; routing
to a real pager and calibrating thresholds against staging traffic remain
deployment responsibilities.

## Docker quick start

```bash
# Idempotently creates JWT keys and the ignored metrics bearer-token file.
make runtime-secrets

# Starts the app profile plus Prometheus, Alertmanager, and Grafana.
make up-observability

# Validates config with digest-pinned promtool/amtool and checks dashboard JSON.
make monitoring-check
```

Local endpoints bind to `127.0.0.1` by default:

- Aero: <http://localhost:8080>
- Prometheus: <http://localhost:9090>
- Alertmanager: <http://localhost:9093>
- Grafana: <http://localhost:3000>

Grafana uses `AERO_GRAFANA_ADMIN_USER` / `AERO_GRAFANA_ADMIN_PASSWORD`; the
checked-in password is a local-only bootstrap value. Production must override it
through the deployment secret manager and must not expose these ports publicly.

## Layout
- `prometheus/recording_rules.yml` — RED recording rules + multi-window 5xx error
  ratios that the burn-rate alerts and dashboard read.
- `prometheus/alert_rules.yml` — multi-window, multi-burn-rate SLO alerts against a
  99.9% HTTP availability target, plus operational alerts on the platform's own
  health gauges (NATS consumer backlog, AI dead-letter queue, dropped poison
  messages, publish errors, DB-pool saturation) — the silent-failure modes this
  codebase has actually hit now page instead of festering.
- `prometheus/prometheus.yml` — Compose scrape + rule-load config.
- `prometheus/prometheus.example.yml` — external-deployment example.
- `alertmanager.example.yml` — example `page`/`ticket` severity routing.
- `grafana/aero-red-dashboard.json` — RED + platform-health dashboard.

## `/metrics` bearer token

`scripts/gen-metrics-token.sh` creates `secrets/metrics_token` without
overwriting an existing value. Compose mounts it into the gateway and Prometheus;
the gateway entrypoint exports it as `AERO_METRICS_TOKEN`, while Prometheus reads
the same file through `authorization.credentials_file`. The value is absent from
both containers' inspectable environment blocks. The `secrets/` directory is
`0700`; the token file is `0644` inside that protected directory because the
unprivileged Prometheus process must read its bind-mounted Docker secret.

## Tuning the SLO
Burn-rate alerts target a 99.9% availability SLO (budget = `0.001`) on the 5xx
ratio. Adjust the budget and the `14.4` / `6` burn multipliers in
`alert_rules.yml` to your target.

## Scope

`make docker-smoke` proves authenticated scraping, rule loading, Alertmanager
health, Grafana datasource/dashboard provisioning and cleanup in an isolated
network. It does not prove production pager delivery, real traffic thresholds,
retention capacity, high availability, or a staging SLO. Those require the
deployment environment and recorded exercise evidence.
