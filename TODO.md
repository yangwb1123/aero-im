# TODO — Autonomous Factory backlog

> Working backlog for the 24×7 factory loop. Strategy/why lives in `ROADMAP.md`
> (第五版); architecture in `AGENTS.md` §1 + `docs/specs/`; history in `git log`.
> Each item: status · scope-class (auto-safe | STOP-needs-decision) · anchor.

## P0 — correctness / deployability (priority #1)

- [x] ~~**BLOCKER — migration chain breaks at 0109 on a fresh DB.**~~ Fixed 40f8cab:
  the colliding `ADD COLUMN tier_id` was dead (no code referenced it), so it was
  dropped outright (DECISIONS.md ADR-002). Full chain now replays clean (125/125),
  guarded by `scripts/migrate_chain_smoke.sh` / `make migrate-smoke` (4495215).
- [x] ~~**db-tests never executed against a full schema.**~~ Swept all crates against
  a fully-migrated DB: aero-storage had 3 broken fixtures (`body`/`email`/`password_hash`
  columns that don't exist) — fixed 57c2d45 (195/0); aero-im-core (11) + aero-server
  (5) were already correct. db-tests now genuinely pass workspace-wide on a fresh DB.
- [x] ~~**gift double-charge (money path).**~~ Fixed c1abddd — optional idempotency
  key (REST `Idempotency-Key` header / WS `nonce`) + partial unique index (mig 0126);
  `insert_gift` ON CONFLICT DO NOTHING, `send_gift` skips broadcast/goals/hype-train
  on a dedup hit. Additive/backward-compatible. Live-verified db-test. ADR-004.
- [x] ~~**bus-listener reconnect black hole.** `run_bus_listener` /
  `run_live_bus_listener` returned permanently when the NATS stream ended on a
  reconnect → silent total fan-out outage.~~ Fixed e5f1fb1 — outer resubscribe loop
  (1s backoff); durable cursor preserves at-least-once. See DECISIONS.md ADR-003.
- [x] ~~GDPR erasure left message `embedding` populated (re-identifiable).~~
  Fixed 984bffc — `embedding = NULL` in `delete_participant` + live-verified db-test.

## P1 — depth / correctness edges

- [x] ~~**legal-hold vs right-to-erasure.**~~ Fixed 0a4be67 — erasure now exempts
  messages under an active legal hold (GDPR Art. 17(3)(e)), mirroring the retention
  sweep. Follow-up: complete erasure on hold-release (needs a deferred-erasure queue).
- [x] ~~End-to-end distributed tracing + SLO surfacing (ROADMAP 第五版 P0-二).~~ Done
  (prior sessions): OTel global `TraceContextPropagator` (telemetry.rs:61), bus
  traceparent stamp/extract (aero-bus/seq.rs), request-id→span, `monitoring/` dir
  (Prometheus recording+alert rules, Grafana RED dashboard, Alertmanager),
  per-tenant metrics. Last-mile scrape/alert wiring is a staging seam.
- [x] ~~Agentic AI + knowledge-base direction (ROADMAP 第五版 P1).~~ Done: agentic
  tool-use loop (aero-ai/agent.rs), file/attachment RAG (doc_extract.rs +
  read_attachment), language-aware FTS (english stemmer + f_unaccent, migs
  0128/0131), saved_search last_run, search total-count + keyset pagination +
  did-you-mean (e8777f6), click-feedback/CTR (7ff81b4). Remaining: persistent
  cross-room AI profile (ambiguous + LLM-quality-gated → product intent) and real
  agentic quality / font-aware PDF (dep+staging seams).
- [x] ~~Data-lifecycle / GDPR-correctness sweep audit (ROADMAP 第五版 P1).~~ Direction
  CLOSED + went beyond the scan via a completeness audit of `delete_participant`:
  message embedding (984bffc), legal-hold exemption (0a4be67), deferred-erasure
  sweep (219aca5), **identity PII** — name/avatar/email/password/phone/profile/SSO
  (50eaa2c), and **authored content** — drafts/OOO/scheduled (f89ad4d). Erasure is
  now comprehensive (identity + all authored content), hold-aware, and eventually
  consistent. Audited-clean: channel-points/predictions spend is atomic (no double-spend).

## P2

- [x] ~~poison-message loop for bus listeners.~~ Fixed 3613b24 — undecodable payloads
  are ack-dropped (not nacked forever) + `aero_bus_poison_dropped_total` metric.
  Broker-side backstop added e392c91/ef39fe9: `poison_safe_pull_config`
  (max_deliver=16, ack_wait=120s) bounds the consumer-CRASH poison class too.
- [x] ~~Auth-abuse depth (ROADMAP 第五版 P2).~~ Done: login lockout (login_throttle.rs),
  magic-byte upload sniff (content_sniff.rs), refresh family-revoke (session.rs:90),
  behavioral spam guard (spam_guard.rs), JWT kid keyring (jwt.rs), and PII detection
  on the send path (pii_detect.rs, 8f82fd6 + extra-text fix 263774f). AV-daemon
  scanning remains a deployment seam (needs ClamAV).
- [x] ~~Data-lifecycle retention for the 4 unbounded heaps + viewer firehose.~~ Done
  18fa965 (notifications/audit_events/ai_jobs/webhook_delivery_log sweeps) + 0fe0236
  (stream_viewer_samples per-minute rollup + downsample, mig 0132). Folded into the
  hourly sweep loop, env-gated windows, live-PG verified.

## Flagged design questions (need product intent, not autonomous fixes)

- **Blocking enforcement scope.** Now enforced at: DM-open (`dm.rs:93`), notification
  fan-out (suppressed — `service.rs:1745`), and **1:1 calls** (`start_call`, 92a2027).
  STILL a product decision for the remaining surfaces: a block does NOT reject
  `send_message` in a pre-existing DM (notifications are suppressed, but the message
  still lands in the room), nor hide a blocked user's content in SHARED rooms, nor
  affect group-DM inclusion / mentions / reactions. If "block = can't open new DM +
  can't call + not notified" is the intended model, this is now complete; if it
  should be a full mute/cloak (content hidden in shared rooms, message-send rejected),
  several surfaces still need a gate. **Needs product decision on the remaining scope.**
  (Audited-clean: AI/RAG retrieval membership-guarded; channel-points spend atomic;
  me_export caller-scoped; blob-download IDOR-guarded; guest isolation enforced.)

- **`created_at` range PARTITIONING of messages + append-only heaps (ROADMAP 第五版
  方向四, the roadmap's own "需独立立项").** The remaining structural XL item. A factory
  hard-STOP (schema break + risky migration on populated data): `messages` is
  referenced by ~20 FKs, and native PG partitioning requires the partition key
  (`created_at`) to be part of the PK — which breaks every FK referencing
  `messages(id)` and forces a full rewrite/lock of the live table. Retention is
  already O(n) via the sweep loop (good enough operationally); partitioning would
  make it O(1) DROP PARTITION but is the single most invasive possible schema change.
  **Needs a deliberate migration project + maintenance window — user decision.**
- **Persistent cross-room AI user profile (ROADMAP 第五版 方向三 低危子项).** AI memory
  is currently per-(participant,room) ephemeral turns. A cross-room *profile* implies
  learned attributes (topics/role/preferences) via an LLM extraction pass — ambiguous
  ("multiple equivalent solutions": raw-turn-log vs extracted-facts vs preference-
  vector), low marginal value, quality staging-gated, and a possible privacy
  regression vs the deliberate per-room scoping. **Needs product intent.**

## Tech debt (see also DECISIONS.md)

- **~~High~~ → mitigated:** migrations never execution-validated (root cause of the
  0109 blocker). `scripts/migrate_chain_smoke.sh` now replays the chain on a fresh
  DB; remaining: wire it into an actual CI workflow (no CI runner in this sandbox).
- **Medium:** dev DB frozen at migration 32 — drift from the 125-migration HEAD
  masks any fresh-deploy schema bug. Mitigated for new bugs by the smoke script, but
  the dev DB itself should be re-provisioned from a clean chain.
- **Low → mitigated:** db-tests were `#[ignore]`-only and never run; now verified to
  pass against a fully-migrated DB workspace-wide. Still no *automated* live-PG CI
  lane (would run `--ignored` against a postgres service).
- **Medium:** no test-AppState harness in aero-server — its 286 lib tests are all
  pure-function, so background tasks (bus listeners, sweeps) and full route flows
  have no unit coverage. Blocks e.g. a resubscribe regression test for ADR-003.
