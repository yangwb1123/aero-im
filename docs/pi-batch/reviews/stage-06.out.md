All evidence gathered. Here is the Stage 06 review.

---

# Stage 06 — Production Readiness Review: Message Recall (撤回)

**Roles:** SRE · DevOps Engineer · QA Lead · Security Engineer
**Subsystem:** Message recall — REST `POST /api/messages/:id/recall` + WS `recall_message`, placeholder semantics
**Repository:** `/home/u1/aero-im` (task's stated path `/home/u1/ai-batch-runner` is the tooling repo; all subsystem files were found under `/home/u1/aero-im`)
**Deployment target / SLO targets:** not provided. Only dev artifacts exist (`docker-compose.yml` = dependencies only, no server image; `scripts/systemd/aero-im-vault.conf` drop-in). Production cutover per `docs/pi-batch/recall-review-context.yaml`: *"production cutover pending"*.
**Prior Critical/High findings:** none supplied to this stage (Stages 02–05 outputs not provided). In-tree pre-merge adversarial review findings (P1 content-resurrection via system edit, P2 change-replay gap, P2 client dedupe, P2 SPA receive-only, HIGH partition backfill) are **verified fixed in the current tree** with regression tests (`docs/pi-batch/recall-completion-report.md` + `system_edit_after_recall_is_fenced`, `changes_since_delivers_recalls`, `partition_backfill_carries_recall_columns`, `concurrent_double_recall_has_exactly_one_winner` present and green).

---

## 1. Runtime component map

| Component | Role in recall journey | State enabled by recall | Multi-replica safe? |
|---|---|---|---|
| **Web SPA** (external frontend, separately deployed) | `web/app.js` recall button → `api.recallMessage` / `ws.recallMessage`; `handleRecalled` → `applyMessageMutation` (out-of-order + resurrect-guarded); `replayChanges` convergence on reconnect | none (client state only) | n/a |
| **Reverse proxy** (assumed, per `AERO_TRUSTED_PROXY_CIDRS` + WS upgrade) | TLS termination, WS upgrade, `X-Forwarded-For` trust gate | none | n/a |
| **aero-server** | REST handler `recall_message` (`routes/handlers/messages.rs`), WS frame `RecallMessage` (`ws/ws_impl/frame.rs:158`), outbox relay (`bin/boot/background.rs`), bus listener `run_bus_listener` (`ws/ws_impl/bus.rs`), `Hub::fan_out_raw` (bounded mpsc per connection) | none (all durable state externalized) | ✅ SKIP-LOCKED leases, per-subject seq in Redis |
| **PostgreSQL 17** | `messages.recalled_at/by`, `message_edits` snapshot, `audit_events`, `event_outbox` (kind `recalled`), `blob_gc_queue` | all recall state | ✅ single source of truth |
| **NATS JetStream** | `im.room.{id}` durable consumer `aero-server`; at-least-once with stable event id | durable cursor | ✅ externalized |
| **Redis** | per-subject seq minting (**fail-open**: outage → gap/unstamped event, never stall) | none | ✅ externalized |
| **Blob store** (LocalFs/S3) | attachment GC after recall (bytes = content, removed) | none | ✅ externalized |
| **Jaeger OTLP / Prometheus** | traceparent stamped through outbox → consumer span; `/metrics` bearer-gated | none | n/a |

**Dependencies:** PG (hard), NATS (hard for fan-out; durable outbox keeps correctness), Redis (soft), blob store (soft). **Secrets:** no new secrets in recall path (JWT/PAT auth as elsewhere). **Ownership:** `ImService` (aero-im-core) + `MessageRepo` (aero-storage) + aero-server handlers + SPA.

**Dependency-loss semantics (verified):** recall commit is a single PG transaction (placeholder, snapshot, audit, outbox row, GC enqueue). NATS down at commit → fast-path publish fails, row re-parks with exponential backoff (outbox relay every 250 ms, `AERO__SERVER__EVENT_OUTBOX_POLL_MS`); event is *delayed, not lost*. Redis down → seq gap, still delivered. Client offline → `changes_since` keys on `GREATEST(edited_at, deleted_at, recalled_at)` (migration 0238 reissues `idx_messages_room_mutated`); slow WS consumers may drop frames but reconnect backfill converges. This design is sound; the gaps are below.

---

## 2. Findings (shared format)

### High

**H1 — Binary rollback to pre-recall poisons the entire room-event outbox relay (decode-fail batch stall); no drain procedure exists**
| Field | Content |
|---|---|
| Surface | nested module |
| Location | `crates/aero-storage/src/event_outbox.rs` — `claim_due` (`rows.into_iter().map(TryInto::try_into).collect()`) + `TryFrom<&str> for EventOutboxKind` (no `Recalled` in pre-0238 enum); no rollback runbook |
| Evidence | A `recalled` outbox row read by a pre-recall binary fails `EventOutboxKind::try_from("recalled")` → whole batch (up to `MAX_CLAIM=500`) returns `Err` → `dispatch_event_outbox_batch` errors every 250 ms tick → **all** room-event fan-out (send/edit/delete/recall) stalls. Rows stay claimed for the 30 s lease, then re-claim and re-fail. Pre-0238 binary also can't publish them. |
| Failure scenario | Deploy 0238 + new binary; a recall's fast-path publish fails (NATS blip) leaving a pending `recalled` row; operator rolls back the binary (the documented pattern in `docs/runbooks/rolling-upgrade-0172-0176.md` is migration-first, old pods drain *after* migration — same window applies) → relay black hole for the whole fleet until rows are manually deleted or a new binary returns. |
| Impact/likelihood | Availability: silent loss of realtime delivery for every room event; no existing metric/alerts on this path (see H2). Likelihood: low (requires rollback during a pending-row window) but consequence severe and currently undetectable except by `warn!` logs. |
| Fix | (a) Make `claim_due` skip-and-flag unknown kinds (fail-open per row + counter) — also future-proofs the next `event_kind`; or (b) runbook: before binary rollback, `SELECT count(*) FROM event_outbox WHERE event_kind='recalled' AND published_at IS NULL` and wait for 0 (or delete those rows, accepting fan-out loss — clients converge via `changes_since`). Add a regression test simulating old-enum decode. |
| Risk/effort | (a) low breaking risk, ~0.5 day; (b) docs-only, hours. |

**H2 — No observability on the event_outbox relay: the single fan-out path can stall silently**
| Field | Content |
|---|---|
| Surface | nested module |
| Location | `crates/aero-common/src/metrics.rs` (no outbox gauge), `crates/aero-server/src/bin/boot/background.rs:82-83` (relay failure = `tracing::warn!` only), `monitoring/prometheus/alert_rules.yml` (`AeroNatsConsumerBacklogGrowing` covers only NATS→server consumer; `AeroNatsPublishErrors` covers publish errors only) |
| Evidence | `NATS_CONSUMER_PENDING_MESSAGES` gauges the *consumer* side; nothing gauges `event_outbox` pending rows or relay batch failures. Recall's delivery guarantee (durable relay) is unmonitored. |
| Failure scenario | H1 stall, or claim-query errors during a PG outage, or relay disabled (`EVENT_OUTBOX_POLL_MS=0`) → `MESSAGES_RECALLED_TOTAL` keeps counting (service-level success) while no client ever receives the `Recalled` frame. No page, no ticket. |
| Impact/likelihood | Availability + SLO blind spot. Likelihood: medium (any relay fault). |
| Fix | Gauge `aero_event_outbox_pending_total` + `aero_event_outbox_oldest_pending_seconds` (bounded query), counter `aero_event_outbox_relay_errors_total`; alert on pending > threshold / oldest > 5 min. Grafana panel. |
| Risk/effort | none; ~0.5–1 day. |

**H3 — Migration 0238 runs blocking DDL on the hot `messages` table inside a transactional migration**
| Field | Content |
|---|---|
| Surface | nested module |
| Location | `migrations/0238_message_recall.sql` — `ADD COLUMN recalled_by UUID REFERENCES participants(id)` (FK validation scan), `DROP INDEX + CREATE INDEX IF NOT EXISTS idx_messages_room_mutated` (no `CONCURRENTLY`), and the full `UPDATE messages_partitioned … FROM messages` reconcile; `crates/aero-storage/src/db.rs:58` (`sqlx::migrate!().run()` — each file runs in a transaction, so concurrent DDL is impossible) |
| Evidence | `docs/runbooks/messages-partitioning.md` establishes `messages` as the partitioned hot table requiring cutover planning. Plain `CREATE INDEX` takes `SHARE`/`ACCESS EXCLUSIVE` for the full build; a `REFERENCES` column constraint validates the whole table. All message writes (send/edit/delete/recall) block for the migration duration. |
| Failure scenario | Production-sized `messages` (millions of rows): index build + FK scan + shadow reconcile take minutes; every message send during that window queues on lock waits → request timeouts → HTTP 5xx burn. |
| Impact/likelihood | Availability during rollout; scales with table size. Likelihood: certain at scale, size unknown (target not provided). |
| Fix | Split: (1) additive columns only (`recalled_by UUID` without REFERENCES, or `NOT VALID` FK), marked `-- migrate:no-transaction`; (2) `CREATE INDEX CONCURRENTLY` + `ALTER TABLE … VALIDATE CONSTRAINT … CONCURRENTLY`; (3) reconcile `messages_partitioned` in bounded batches (the existing `backfill_messages_partition` pattern). Measure migration time on a staging copy first. |
| Risk/effort | Low breaking risk (SQL-only), ~1 day + staging timing test. |

### Medium

**M1 — Recall endpoint is an existence oracle, contradicting its own documented contract**
| Field | Content |
|---|---|
| Surface | nested module |
| Location | `crates/aero-im-core/src/service/messages.rs` — `recall_message` (`get` → 404 before `assert_room_access` → 403); doc comment claims "no existence oracle". Same shape in edit/delete/GET (pre-existing), while `reactions_batch` (`routes/handlers/messages.rs`) uses the codebase's own silent-absence pattern. |
| Evidence | `MessageId` is a time-sortable ULID (`crates/aero-common/src/ids.rs`). Any authenticated user can distinguish 404 (nonexistent) from 403 (exists, inaccessible) and thus enumerate cross-workspace message existence and timing. |
| Failure scenario | Member of workspace A probes `/api/messages/:id/recall` with ULIDs harvested/guessed from workspace B → learns which messages exist and their creation order. |
| Impact/likelihood | Information disclosure (existence/metadata only, no content); low likelihood, low severity — but the code comment asserts a guarantee the code does not deliver. |
| Fix | Collapse inaccessible → 404 (mirror `reactions_batch`), or correct the doc comment and record an accepted risk. Check web client 403 handling before shipping the 404 change. |
| Risk/effort | Small; minor client-contract risk. |

**M2 — Recall (REST + WS) has no rate limit or slow-mode gate**
| Field | Content |
|---|---|
| Surface | nested module |
| Location | `crates/aero-server/src/routes/handlers/messages.rs` (`recall_message` — no `check_ws_rate_room`), `crates/aero-server/src/ws/ws_impl/frame.rs:158` (`RecallMessage` — no rate check); contrast edit/send which gate at `frame.rs:146,1098` |
| Evidence | Each attempt costs ~4–6 queries (get + room access + role read + tx); author attempts additionally write snapshot+audit+outbox. Attempts on others' messages are cheap 403s but unthrottled per connection. Delete shares the gap (pre-existing). |
| Failure scenario | Scripted client fires recall frames at the WS rate cap → DB query amplification per connection; multiple connections multiply. |
| Impact/likelihood | Availability/DoS (bounded by connection limits, so low severity); inconsistent enforcement on a new endpoint. |
| Fix | Apply `check_ws_rate_room` on recall/delete frames + REST, or a per-actor recall-attempt limiter. |
| Risk/effort | Low; ~0.5 day. |

**M3 — No end-to-end integration test of the recall journey (HTTP → outbox → NATS → WS fan-out)**
| Field | Content |
|---|---|
| Surface | nested module |
| Location | Test inventory: storage `recall_tests.rs` (8 DB tests incl. race/backfill/redaction), im-core `db_tests/recall_tests.rs` (service level), `web/{ws,api,render_recall}.test.js`; `relay_tests.rs` covers only Notify; no `scripts/smoke_*.py` mentions recall; CI `ci.yml` runs integration suite without recall coverage |
| Evidence | The fan-out path's `Recalled` handling is unit-tested at frame serialization (`ws/frame.rs:225`) and outbox materialization (`outbox.rs`), but no test drives recall through two clients (author recalls → peer converges; admin recalls another's message; offline peer replay via `changes_since`). |
| Failure scenario | A regression in bus listener / hub fan-out for `Recalled` passes all gates but breaks realtime recall delivery. |
| Impact/likelihood | QA/regression gap; likelihood of drift medium. |
| Fix | Add a two-client smoke (extend `scripts/ws_smoke.py` pattern): send → recall via REST and WS → assert peer frame + `changes_since` replay + 409 mapping. |
| Risk/effort | None; ~1 day. |

### Low / Info

**L1 — No kill switch for recall.** No feature flag; the only disable path is code rollback (which carries H1). Fix: env-gated route/frame or proxy-level route block. Effort: hours.
**L2 — No backup/restore or fault-injection drill evidence anywhere in repo** (`docs/runbooks/` has partitioning + rolling-upgrade only; no pg_dump/restore runbook; `messages-partitioning.md` *requires* "a tested restore" before cutover but no drill log exists). Required before production go.
**L3 — Unpinned images in the only published deployment artifact:** `docker-compose.yml` uses `jaegertracing/all-in-one:latest`, `minio/minio:latest` (dev-only; server itself has no image at all). Pin or document as dev-only.
**L4 — Recall semantics ≠ erasure (by design):** pre-recall body remains room-member-readable via `GET /api/messages/:id/history` (`message_history.rs`, room-access gated) and a 120-char digest lands in `audit_events` (same as delete). GDPR erasure does cover `message_edits` (`participant.rs:575,660`; `message_edit.rs:307`). Product decision, but must be stated in the compliance sheet; `message_edits` has no retention sweep (unbounded growth per recall/edit).

---

## 3. Release checklist

| Item | Verdict | Evidence | Owner |
|---|---|---|---|
| Migration 0238 applied on fresh + existing DB; replay on throwaway DB | PASS | CI `aero_migrate_smoke` + `aero_rollout_smoke` (fresh-DB 238-migration replay); idempotent `IF NOT EXISTS` | DevOps |
| Migration lock-window sized on production-shaped data | **NEEDS WORK** | H3; no staging timing data | DBA/SRE |
| Recall schema + index + shadow columns verified | PASS | `recall_schema_columns_and_outbox_kind_are_applied`, `partition_backfill_carries_recall_columns` | QA |
| Permission matrix (author/admin/owner/member, cross-workspace, double-recall race) | PASS | 8 storage + service integration tests, incl. `concurrent_double_recall_has_exactly_one_winner` | QA |
| Content-resurrection fences (system edit / transcribe after recall) | PASS | `system_edit_after_recall_is_fenced`; `recall_snapshot_redacts_blob_references_and_gc_proceeds` | QA |
| Reconnect convergence (`changes_since` includes recalls) | PASS | `changes_since_delivers_recalls`; index reissued in 0238 | QA |
| SPA recall UX (button, placeholder render, 409→success, terminal state) | PASS | `web/{api,ws,render_recall}.test.js` (76/0 node tests) | FE/QA |
| HTTP/WS recall rate limiting | **FAIL** | M2 — no gate on either path | Backend |
| Outbox relay observability (pending gauge + alert) | **FAIL** | H2 — no gauge, no alert | SRE |
| Binary-rollback drain procedure for `recalled` outbox rows | **FAIL** | H1 — no runbook; decode stall proven by code | SRE/DevOps |
| Readiness/liveness/drain | PASS | `/health{,/live,/ready}` probes PG/Redis/NATS/blob (2 s timeouts); draining → 503; `AERO_TASK_DRAIN_SECS` (default 30) | SRE |
| Telemetry: trace continuity | PASS | traceparent captured → outbox row → wire stamp → consumer span (`outbox.rs:335`, `bus.rs:135-143`) | SRE |
| Cardinality-safe metrics, no content in labels | PASS | route label from `MatchedPath`; workspace label gated by `per_tenant_metrics_enabled`; no message content in metrics | SRE |
| Sensitive-data handling (audit digest, redacted snapshot) | PASS | snapshot redacts `blob_id`s; audit digest ≤120 chars (matches delete path) | Security |
| TLS / proxy trust | N/A | terminated at assumed reverse proxy; `AERO_TRUSTED_PROXY_CIDRS` enforced for forwarded headers | DevOps |
| Image/dependency pinning | **NEEDS WORK** | L3 (`:latest` in compose); `Cargo.lock` + `rust-toolchain.toml` + `deny.toml` pinned | DevOps |
| Canary/kill switch | **NEEDS WORK** | L1 — no feature flag; wave rollout possible (drain exists) | DevOps |
| Backup/restore runbook + drill | **NEEDS WORK** | L2 — no runbook, no drill evidence | SRE |
| Fault-injection/staging evidence | **NEEDS WORK** | No staging env, no NATS-down drill for recall | QA/SRE |
| E2E two-client recall smoke | **NEEDS WORK** | M3 | QA |

---

## 4. SLO table (proposed — no targets were provided; all formulas map to existing metrics)

| User signal | SLI formula/source | Target/window | Alert |
|---|---|---|---|
| Recall accepted (REST) | `1 − Σ5xx / Σ` on `aero_http_requests_total{route="/api/messages/:id/recall"}` (existing route-labeled counter + histogram) | 99.9% / 30 d (align with existing `aero-slo-burn` group) | Extend `recording_rules.yml` with a route-scoped error ratio, reuse burn-rate alerts (NEEDS WORK) |
| Recall latency | `aero_message_processing_duration_seconds{op="recall"}` p99 (existing histogram) + per-route HTTP p99 | p99 < 1 s / 5 m | `AeroHttpLatencyP99High` (global) + route p99 alert (NEEDS WORK) |
| Recall fan-out delivered | `rate(aero_messages_recalled_total)` vs `aero_nats_consumer_pending_messages{consumer="aero-server"}` + **new** `aero_event_outbox_pending_total` (H2) | backlog < 1000, outbox pending ≈ 0 sustained | `AeroNatsConsumerBacklogGrowing` exists; outbox alert missing (H2) |
| Realtime availability | `/health/ready` PG/NATS probes + HTTP error budget | 99.9% / 30 d | existing burn-rate + `AeroNatsPublishErrors` |
| Convergence after reconnect | No direct metric — proxy: `changes_since` latency from route histogram | p99 < 1 s | route p99 alert (NEEDS WORK) |

---

## 5. Top three runbooks

**RB1 — "Recalls return 200 but peers still see original content" (fan-out black hole)**
- **Symptom:** REST/WS recall succeeds (`aero_messages_recalled_total` increasing) but no client renders the placeholder; logs: `message event outbox relay batch failed` or `message outbox publish failed`.
- **Diagnosis:** `SELECT count(*) FROM event_outbox WHERE published_at IS NULL;` (nonzero = relay stalled); `SELECT event_kind, attempts, available_at FROM event_outbox WHERE published_at IS NULL ORDER BY available_at LIMIT 20;` (stuck `recalled` rows + decode errors = H1); check NATS: `nats consumer report` / `aero_nats_publish_errors_total`; check `AERO__SERVER__EVENT_OUTBOX_POLL_MS` not 0.
- **Remediation:** NATS down → restore NATS (relay self-heals via backoff); decode stall → ensure all pods run the post-0238 binary, then restart one pod to re-claim (30 s lease); if a pre-recall pod is present, drain it first.
- **Verification:** pending count → 0; a test recall observed on a second client within poll interval.
- **Rollback boundary:** none — DB stays; do not roll back the binary (H1).
- **Escalation:** SRE on-call → IM owners (aero-im-core).

**RB2 — "Rolled back the binary; all realtime stopped" (H1)**
- **Symptom:** after binary rollback, no room events fan out anywhere; repeated `unknown event_outbox event_kind "recalled"` decode warnings.
- **Diagnosis:** `SELECT count(*) FROM event_outbox WHERE event_kind='recalled' AND published_at IS NULL;`
- **Remediation:** (fast) redeploy post-0238 binary and let it publish pending rows; (forced) if rollback must stand: `DELETE FROM event_outbox WHERE event_kind='recalled' AND published_at IS NULL;` — accepted loss: affected clients converge via `changes_since` on next reconnect.
- **Verification:** pending count = 0; relay log clean; live recall probe works or is intentionally disabled.
- **Rollback boundary:** this runbook *is* the rollback; DB stays at 0238 (additive — old binary ignores new columns).
- **Escalation:** SRE on-call + IM owners; data-loss decision (DELETE) requires sign-off.

**RB3 — "Deploying 0238: message sends timing out" (H3 lock window)**
- **Symptom:** during `aero-cli migrate`, sends/edit/recalls queue; `pg_stat_activity` shows `CREATE INDEX` / `ALTER TABLE` on `messages` with long lock wait.
- **Diagnosis:** `SELECT pid, state, wait_event_type, query FROM pg_stat_activity WHERE query ILIKE '%idx_messages_room_mutated%' OR query ILIKE '%recalled%';` size the table: `SELECT pg_size_pretty(pg_total_relation_size('messages'));`
- **Remediation:** planned maintenance window for the first deploy; migration is transactional + idempotent (`IF NOT EXISTS`) so it can be re-run; if it must be aborted mid-window, kill the session — the failed version is not recorded (`rolling-upgrade-0172-0176.md` §4 pattern). Permanent fix per H3 (concurrent DDL) before general rollout.
- **Verification:** `SELECT version, success FROM _sqlx_migrations ORDER BY version DESC LIMIT 3;` index present; sends healthy after deploy.
- **Rollback boundary:** DB forward only (additive); abort = rerun later.
- **Escalation:** DBA + SRE.

---

## 6. Rollout and rollback/forward-fix procedures

**Rollout (ordered; ~1.5–3 h):**
1. **Preflight (30 min):** PG backup verified restorable (L2); `_sqlx_migrations` at 0237; `event_outbox` pending = 0; record baseline of `aero_messages_recalled_total`/NATS backlog; confirm `AERO_TASK_DRAIN_SECS` ≥ 30 and readiness probes wired to LB.
2. **Build (10 min):** `cargo build --locked` both `aero-cli` and `aero-server` from the exact source (migrations compile-time embedded).
3. **Migrate (TBD by H3 sizing):** apply chain through 0238 on canary host in maintenance window; verify columns/index; run one recall round-trip against migrated DB.
4. **Canary (20 min):** deploy new binary to one host; verify `/health/ready`, REST recall 200, WS recall frame observed on a second client.
5. **Waves (20–30 min each):** 20 % → 50 % → 100 %. Per wave: mark old pods draining (readiness 503) → SIGTERM → graceful drain (`AERO_TASK_DRAIN_SECS`) → deploy → verify. Keep ≥1 new pod serving during each wave so pending `recalled` rows are published promptly.
6. **Post-deploy (30 min):** `event_outbox` pending ≈ 0 sustained; backlog < threshold; `MESSAGES_RECALLED_TOTAL` climbing; SPA E2E recall on 2 clients; error budget burn < threshold.

**Rollback / forward-fix (30–60 min):**
- **Preferred: forward-fix** — ship a new binary; DB never reverts (0238 additive).
- **Binary rollback:** *only after* `SELECT count(*) FROM event_outbox WHERE event_kind='recalled' AND published_at IS NULL` = 0 (or execute RB2's forced DELETE with sign-off); then reverse the waves (drain new → start old). Old binary tolerates the new schema (explicit column lists; serde defaults).
- **DB rollback:** not offered — dropping `recalled_at/by` destroys recall state and requires rebuilding the CHECK constraint and index; data-loss acceptance only, never as routine.

---

## 7. Validation performed vs required, residual risks, decision

**Performed (observed in this review, current tree):** permission matrix unit + integration tests; double-recall race (exactly one winner/outbox row); cross-workspace isolation; placeholder/audit/snapshot/GC transaction contents; system-edit/transcribe fences; `changes_since` replay; partition-backfill column carry; blob-redaction + GC; web tests (API/WS/render, 76/0); full CI gates (2157 lib tests, 593 ignored integration incl. fresh-DB migration replay, web-check, truth-check, file-size-check, clippy `-D warnings`).

**Still required (no evidence in repo):** production-scale migration timing; two-client E2E smoke (M3); fault injection (NATS down mid-recall; relay kill/restart); backup restore drill (L2); staging deployment (target not provided); load/capacity test of the recall path (audit + snapshot + outbox writes per recall at rate).

**Residual risks:** H1/H2 (undetected, unrecoverable-by-design rollback stall + silent fan-out black hole), H3 (deploy-time write stall at scale), M2 (unthrottled new endpoint), L4 (evidence retention growth, no `message_edits` sweep).

### Decision: **Conditional Go**

**Conditions (must-fix before general production rollout):**
1. **H2** — outbox pending gauge + relay-error counter + alert (prerequisite for operating any of this).
2. **H1** — either skip-unknown-kind decode in `claim_due`, or publish the RB2 drain procedure; add rollback-compat regression test.
3. **H3** — split 0238 into concurrent-safe DDL (`-- migrate:no-transaction`, `CREATE INDEX CONCURRENTLY`, `NOT VALID`/`VALIDATE CONCURRENTLY` FK, batched shadow reconcile) and measure on a staging-sized table.
4. **M2** — rate-gate the recall REST route and WS frame (delete while there).
5. **M3** — two-client recall E2E smoke in CI.

**Explicitly deferred (acceptable with sign-off):** M1 (fix doc-comment contract or collapse 403→404 — decide before public launch), L1 kill switch (proxy-route block suffices initially), L4 retention sweep for `message_edits`, L3 pinning (dev-only artifact).

**Validation vs inference:** all findings above cite specific code paths, tests, or config present in the tree; the only inferences are severity/likelihood ratings where the deployment target, table cardinality, and SLO targets are unknown (stated in each finding). The subsystem's correctness design (transactional outbox, row-lock fences, replay convergence) is production-grade; the readiness gaps are operational — observability, migration ergonomics at scale, and rollback safety.
