All verification complete. Both required inputs are **absent from the repository** — `prompts/README.md` does not exist (no `prompts/` directory), and `ops/deploy/` does not exist (no `ops/` directory anywhere, including in any depth). I verified every deployment-adjacent asset in-repo rather than inferring from `docker-compose.yml` (which is dev-dependency-only: Postgres/Redis/NATS/Jaeger/MinIO — **not** the server). Full report follows.

---

# DevOps Review — Aero IM (recall round-4 evidence)

## 1. Supported deployment-path and artifact inventory

| Asset | Location | Status | Notes |
|---|---|---|---|
| CI workflow | `.github/workflows/ci.yml` | **Present, green-path only** | 7 jobs: build, msrv 1.80, check+clippy, test, integration (PG/Redis/NATS services + fresh-DB migration-chain smoke + rolling-upgrade regression), gates (`aero-cli gate all` + npm test + eslint), security (cargo-deny). All `--locked`. **No artifact upload, no container build, no release/deploy stage, no tag triggers.** |
| Migrations | `migrations/` (238 SQL) | Present | Compile-time embedded (`sqlx::migrate!("../../migrations")`) — binary must be rebuilt from exact source. `0238_message_recall.sql` is **untracked** (`??` in `git status`). |
| Server binary | `crates/aero-server` → `aero-server`, `aero-cli` | Buildable | CI builds **debug** only; no `--release` artifact anywhere. `aero-cli` is the ops surface: `migrate`, `health`, `check`, `smoke`, `gate`, `test`. |
| Runtime manifests | `docker-compose.yml` | Dev deps only | No `Dockerfile` for `aero-server` exists (verified: none at repo root or crates/). |
| Service supervision | `scripts/systemd/aero-im-vault.conf` | **Partial** | Drop-in fragment only (`Wants=aero-vault.service`, `EnvironmentFile=/etc/aero-im/aero-im-integrations.env`). **The base `aero-server` unit and Vault unit are not in-repo.** |
| Health/probes | `routes/health.rs`: `/health`, `/health/live`, `/health/ready` | Present, well-built | Probes PG/Redis/NATS/blob (2s timeouts), draining → 503, readiness policy unit-tested. |
| Observability | `monitoring/` (Prometheus rules, example scrape config, Alertmanager example, Grafana dashboard) | Present as configs | README states wiring to a live cluster is the deployment step — **examples, not wired**. `/metrics` bearer-gated (`AERO_METRICS_TOKEN`). |
| Runbooks | `docs/runbooks/`: `rolling-upgrade-0172-0176.md`, `messages-cutover.sql` + `messages-cutover-rollback.sql`, `messages-partitioning.md` | Present | Migration-specific; **no general deploy/rollback runbook**. |
| Config delivery | `config.example.toml`, `config.toml`, `.env.example`; `AERO__SECTION__KEY` env vars | Present | Parity drift: example=8080, checked-in `config.toml`=3030. |
| Secrets | `secrets/jwt_private.pem`/`jwt_public.pem` (dev), `scripts/gen-jwt-keys.sh` | Dev-only | No KMS/Vault role or secret definitions in-repo; no JWT rotation procedure. |
| Dependency security | `deny.toml` + CI `cargo deny check` | Present | Advisories via network DB (not pinned); no secret scanning, no image scanning (no images). |
| Reproducibility | `Cargo.lock` committed, `--locked` CI, `vendor/` MSRV snapshots | Present | Good. |
| Release metadata | git tags | **None** (`git tag` empty) | No versioning, no provenance/attestation, no SBOM. |

**Supported deployment path today:** source → CI gates → local `cargo build --release` + `cargo run --bin aero-cli -- migrate` on a host → manual `systemd`-style process supervision → manual migration-first rollout per runbooks → manual probe via `/health/ready` + `scripts/smoke*.py`. Nothing beyond that is automated or even fully specified in-repo.

## 2. Pipeline table

| Stage | Current evidence | Gap | Proposed gate | Owner |
|---|---|---|---|---|
| Source control | `main` + feature branches; batch-harness commits | Recall feature (incl. migration 0238) **uncommitted** | Merge + `main` CI green before any deploy | Feature owner |
| CI verification | 7-job workflow; gates job runs `aero-cli gate all` + web tests; fresh-DB 238-migration replay; rolling-upgrade regression | CI never ran on recall code; evidence gates were local-only | Add recall regression to CI's ignored suite (as done for 0172–0176) | CI owner |
| Artifact build | Debug builds only | **No `--release` build, no container image, no Dockerfile, no artifact retention/checksums** | `cargo build --release --locked` job + publish image (or tarball + sha256) to a registry | Platform team |
| Artifact registry / provenance | — | None; no SBOM, no attestation, no versioned tags | Registry + signed manifests; tag on release branches | Platform team |
| Env promotion (dev→staging→prod) | — | No staging env definition; no promotion automation | Staging = full stack incl. real network path (see §5) | Platform team |
| Deploy | Migration-first runbook (0172–0176) + `aero-cli migrate`; systemd drop-in fragment | No IaC (`ops/deploy/` missing), no base unit, no rollout orchestration, no canary/blue-green decision | `ops/deploy/` with units/IaC; ordered rolling with drain; version-skew check | Platform team |
| Post-deploy verify | `/health`, `/health/live`, `/health/ready`; `scripts/smoke*.py` suite | No automated post-deploy smoke; monitoring not wired | Automated probe + smoke gate wired to metrics | SRE |
| Rollback | `messages-cutover-rollback.sql` (single migration) | No general binary/migration rollback procedure; no restore drill | Rollback runbook + tested restore; feature flags | SRE |

## 3. Findings (severity-ordered)

**F1 — CRITICAL — No deployment assets exist (`ops/deploy/` absent).** The prompt's own verification requirement returns nothing: no IaC, no units, no container build, no environment definitions. Production impact: there is no supported deployment path to production; "deployability" cannot be claimed from `docker-compose.yml` (dev dependencies). Remediation: create `ops/deploy/` with a concrete target topology (single-node systemd or k8s), matching the design's "single instance → P5+ cluster" reality (`docs/specs/2026-05-22-aero-im-design.md` line 248). Validation: a fresh host can provision the full stack from `ops/deploy/` + runbooks alone.

**F2 — CRITICAL — Recall feature is uncommitted; CI never ran on it.** Verified: `?? migrations/0238_message_recall.sql` and ~30 modified files; no git tags; evidence states "Nothing committed". Local gates (2159 tests, 238-migration replay on throwaway DBs) are real but not a CI artifact. Production impact: any deploy would be of `main` **without recall**; conversely, shipping the working tree would bypass CI. Remediation: commit feature + migration, let CI run, treat `main` green as the promotion gate. Validation: CI `integration` job passes including the 10/10 recall storage suite.

**F3 — HIGH — No release pipeline / reproducible artifact.** No `--release` build in CI, no Dockerfile, no registry, no tags, no SBOM/provenance. Production impact: unverifiable what binary is running; migration-first discipline (compile-time-embedded SQL) makes binary↔source coupling load-bearing and unprovable post-hoc. Remediation: release job producing versioned, checksummed artifact from a tagged commit; record `git rev-parse` in the deploy record. Validation: deploy log shows artifact hash == CI hash.

**F4 — HIGH — No backup/restore for stateful stores.** No `pg_dump`/WAL automation, no Redis AOF/RDB policy, no NATS JetStream backup (durable consumer cursor `aero-server` is a replay/ordering fact source — losing it means at-least-once redelivery with dedup keys, not data loss, but stream data loss is unrecoverable). `data/` are dev docker volumes. Remediation: nightly PG dump + JetStream snapshot, restore drill, document RPO/RTO. Validation: quarterly restore drill on throwaway env.

**F5 — HIGH — Secrets delivery is a fragment.** Only a systemd drop-in referencing `/etc/aero-im/aero-im-integrations.env` and an external `aero-vault.service`; no Vault role/secret definitions, no JWT key rotation, `secrets/` holds dev keys. Production impact: operator hand-jobs secrets; JWT RS256 key compromise = full session forgery (JWT fallback also accepts `aero_pat_*`). Remediation: define Vault/KMS paths for `AERO__AUTH__JWT_PRIVATE_KEY_PEM`, `AERO_METRICS_TOKEN`, S3 keys, SMTP/OIDC creds; rotation runbook. Validation: secret-free deploy from scratch.

**F6 — MEDIUM — Version-skew on new `RoomEvent::recalled` variant is documented, not enforced.** `docs/pi-batch/message-recall-plan.md` DS-4: old binaries ack-drop unknown `kind:"recalled"` and the durable cursor advances — **permanent event loss**. Remediation: enforce all-nodes-upgrade-first in the deploy gate (e.g., minimum-version check before traffic cutover; or map unknown kinds to a poison-free "ignore but ack" path in the next release). Validation: mixed-fleet test — old binary + recalled event → event must not be silently dropped from the durable consumer for new readers.

**F7 — MEDIUM — Monitoring/tracing are configs, not wiring.** Prometheus/Alertmanager configs are examples; SLO alerting has no owner/routing; OTLP cross-process tracing explicitly unverified without a live collector. Remediation: wire scrape + alertmanager routing, define on-call ownership, verify `traceparent` end-to-end. Validation: alert fires on injected 5xx; trace spans cross NATS boundary.

**F8 — MEDIUM — Env parity drift.** `config.toml` port 3030 vs `config.example.toml` 8080 (AGENTS.md documents this as known). Production impact: config drift causes deployment confusion (the exact class of issue that produced the "8080 vs 3030" smoke failures). Remediation: production config built from env overrides only; example file canonical. Validation: smoke scripts pass against prod-shaped env without `config.toml`.

**F9 — LOW — Security scanning gaps.** cargo-deny advisory DB is network-fetched (not pinned); no secret scanning (trufflehog/gitleaks) in CI; no container scanning (no images). Remediation: pin/periodically update advisory DB with review loop; add secret scan. Validation: CI job fails on planted secret.

**F10 — LOW — No load/chaos testing; multi-replica topology unverified.** Design doc treats multi-node as P5; architecture (NATS durable consumers, Redis sorted-set cluster state, readiness probes) is horizontal-scale-ready, but the only multi-instance evidence is localhost dual-gateway call-bridge tests. Not a blocker for single-node production; a blocker for a multi-replica claim.

## 4. Release and rollback procedure (as executable today, with gaps flagged)

**Release ordering (recall round):**
1. **Preflight**: `git status` clean-on-main; CI `integration` green (includes fresh-DB 238-migration replay + recall storage suite 10/10); `aero-cli gate all` + npm test green on the tagged commit; backup taken (see F4 — currently manual).
2. **Build**: `cargo build --release --locked` from the exact tagged source; record binary sha256 + `git rev-parse HEAD` (F3 gap: no CI artifact).
3. **Migration-first**: deploy new `aero-cli`, run `aero-cli migrate` (238-chain; 0238 is purely additive Expand–Migrate–Contract, re-runnable, no backfill). Verify `_sqlx_migrations` success count == 238. This is the only migration ordering rule that matters for recall.
4. **Deploy code**: stop old server (graceful drain — `CancellationToken` drain + `/health/ready` → 503), install new binary, start. **All nodes must upgrade before any serves traffic** (DS-4 version skew — F6).
5. **Probes**: `/health/live` → `/health/ready` (PG/Redis/NATS/blob 200); `/metrics` bearer-gated check; WS ping (`aero-cli ws-ping`); smoke suite (`scripts/smoke_*.py`, at minimum delivery cursor + a recall WS flow — recall has no browser-E2E, covered by frame-contract + storage tests).
6. **Post-deploy**: watch SLO burn-rate alerts (once monitoring wired, F7); verify `MESSAGES_SENT_TOTAL` monotonicity; confirm no NATS consumer backlog growth (`observability_gauge_samplers`).
7. **Rollback**: for 0238 — no rollback needed (additive, null-defaulted columns; old binary reads them harmlessly); general policy: keep previous release binary + snapshot; restore = stop, restore PG from pre-migration dump (F4 gap), restore JetStream, start old binary. `messages-cutover-rollback.sql` is the only in-repo example of a real reversal.

## 5. Missing assets / operator decisions blocking a production claim

1. **The `ops/deploy/` directory itself** — zero IaC, zero units, zero environment definitions. Everything else is secondary.
2. **Container image or release artifact pipeline** — no Dockerfile, no `--release` CI build, no registry, no tags/versioning, no provenance.
3. **Secrets management** — Vault integration is a fragment referencing out-of-repo assets; no JWT key rotation; no `AERO_METRICS_TOKEN` distribution plan.
4. **Backup/restore automation + drill** — none for PG/Redis/JetStream; no RPO/RTO stated.
5. **Staging environment** — the design's own staging list (real WebRTC devices, cross-host NAT/TURN, real ffmpeg/OBS/SRT networks, real S3/FCM/APNs/SMTP/OIDC credentials) has no defined environment or entry criteria.
6. **Rollout automation + version-skew gate** — recall's ack-drop hazard is documented discipline only; needs an enforced gate (or a code-level fix).
7. **Feature commit** — recall (code + migration 0238 + web test list) is uncommitted; CI has never evaluated it. This alone blocks any "release" claim for the feature.
8. **Operator decisions**: recall window limit (currently unbounded — retention/legal exposure), admin-recall surface confirmation, SLO/on-call ownership for the shipped alert rules, whether multi-replica (P5) is in scope for first production, and `AERO_DATABASE__REPLICA_URL` read-replica usage in production.

**Bottom line**: engineering gates for the recall feature are genuinely strong (hermetic red-first tests, fresh-DB migration replay, version-skew awareness), but this repository has **no deploy path, no release pipeline, and no ops assets** — and the feature itself is not yet committed. A production claim requires F1–F3, F5, and the commit gate to be closed first.
