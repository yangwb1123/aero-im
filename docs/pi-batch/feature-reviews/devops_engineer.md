Review complete — written to `docs/pi-batch/feature-reviews/devops_engineer.md`. All claims labeled Verified/Missing/Unknown; every finding cites a path I inspected.

## Headline

**`ops/deploy/` does not exist in this repository** — no `ops/` path anywhere in the tree. There is **no supported production deployment path**: no Dockerfile, no systemd units (only a dangling 10-line vault drop-in referencing a non-existent base unit), no IaC, no release workflow, no artifact store, no backup automation. The tree is deployable to a developer machine only (`make up` → `cargo build` → `aero-cli migrate` → foreground `aero-server`).

## Key verified facts

- **CI** (`.github/workflows/ci.yml`, 6 jobs) is real and mostly strong: `--locked` builds, MSRV 1.80, integration job with PG/Redis/NATS + fresh-DB migration replay + rolling-upgrade fence regression, cargo deny.
- **Runtime primitives exist**: `/health/ready` probes PG/Redis/NATS/blob and 503s while draining (`AERO_SHUTDOWN_DRAIN_SECS`); per-instance NATS durables keyed by `AERO_INSTANCE_ID`; monitoring/ has SLO burn-rate alerts + dashboard; 238 one-way migrations, compile-time embedded.

## Top findings

| # | Sev | Finding |
|---|---|---|
| C1 | **Critical** | No deployment path; `ops/deploy/` absent; nothing promotable |
| H1 | High | CI never builds release profile (debug only); no provenance/SBOM/signing |
| H2 | High | CI clippy lacks `-D warnings` → cannot fail on new warnings, contradicts AGENTS §4.2 hard rule |
| H3 | High | Migrations run on pool with `statement_timeout=10s` (`aero-storage/db.rs`) — 0238-scale `CREATE INDEX` can abort mid-chain at scale; CI only proves empty-DB replay |
| H4 | High | `/changes` clamped to 200, client never loops → >200 mutations per reconnect window (recalls incl.) lost forever; likely during rolling deploys |
| M1–M6 | Med | No backup/RPO-RTO; secret delivery unrepresented + **live GitHub PAT embedded in `.git/config`**; `/metrics` unauthenticated unless `AERO_METRICS_TOKEN` set (docs overstate the gate); `web/package.json` `npm test` silently omits `render_recall.test.js` (15 files exist, 14 listed); instance-id uniqueness is convention-only; `docs/engineering-cli.md` documents non-existent `aero-cli` commands |

## Blockers for a production claim (top of §5)

1. The `ops/deploy/` layer itself (image/units, IaC, release→promote→deploy workflow)
2. Artifact provenance (release build, git stamp, SBOM)
3. Migration-at-scale strategy (statement timeout, index-build timing)
4. `/changes` continuation fix
5. Backup/restore + RPO/RTO, secret delivery, staging env with 2+ nodes (distinct `AERO_INSTANCE_ID`), HA decision, observability wiring, load validation

**Verdict**: application gates are sound and green (per implementer evidence); the blockers are entirely in the missing delivery layer — ordered C1 → H1/H2 → H3/H4 → M-items in the report.
