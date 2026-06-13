# Aero IM monitoring (ROADMAP5 方向二: the metrics → SLO layer)

In-repo Prometheus / Grafana / Alertmanager config for the platform's RED signals
and error-budget alerting. Every rule here is **grounded in metrics the server
actually emits** (`crates/aero-common/src/metrics.rs`,
`crates/aero-server/src/metrics.rs`) — so this is buildable and reviewable in the
repo; wiring it to a live cluster (real scrape + alert routing) is the deployment
step (the "last mile" the design notes is staging-gated).

## Layout
- `prometheus/recording_rules.yml` — RED recording rules + multi-window 5xx error
  ratios that the burn-rate alerts and dashboard read.
- `prometheus/alert_rules.yml` — multi-window, multi-burn-rate SLO alerts against a
  99.9% HTTP availability target, plus operational alerts on the platform's own
  health gauges (NATS consumer backlog, AI dead-letter queue, dropped poison
  messages, publish errors, DB-pool saturation) — the silent-failure modes this
  codebase has actually hit now page instead of festering.
- `prometheus/prometheus.example.yml` — example scrape + rule-load config.
- `alertmanager.example.yml` — example `page`/`ticket` severity routing.
- `grafana/aero-red-dashboard.json` — RED + platform-health dashboard.

## `/metrics` is bearer-gated
The endpoint requires a bearer token (`AERO_METRICS_TOKEN`); give the same token to
the Prometheus scrape job via `bearer_token_file`.

## Tuning the SLO
Burn-rate alerts target a 99.9% availability SLO (budget = `0.001`) on the 5xx
ratio. Adjust the budget and the `14.4` / `6` burn multipliers in
`alert_rules.yml` to your target.

## Scope
This is the **metrics → SLO** half of 方向二. The **distributed-tracing** half
(NATS `traceparent` propagation, OTel `TextMapPropagator`, request↔trace pivot) is
separate: the request-id→log correlation is wired (see `inject_request_id`), but
cross-process trace linkage needs a live OTLP collector to verify end to end.
