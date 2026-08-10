# Requirements Spec — aero-audit-connector: leased-relay state machine (ai_usage claim/settle/requeue + claim_token fencing + 300s backoff cap) + UsageSink reserve/finalize/cancel idempotency protocol

- **Module**: `crates/aero-ai` (pattern source, read-only — no aero-ai code changes) → deliverable is the **new** relay crate `crates/aero-audit-connector` (B5-2)
- **Direction**: "Build aero-audit-connector on the proven in-repo leased-relay state machine (ai_usage_outbox claim/settle/requeue + claim_token fencing + 300s backoff cap) and the aero-ai UsageSink reserve/finalize/cancel idempotency protocol"
- **Source analysis**: `docs/auto/analyses/crates-aero-ai-f8cd3622.json` (direction #2; value 8 / risk-reduction 9 / effort 6 / confidence 8)
- **Campaign**: `aero-im-b5-outbox-relay` (`docs/campaigns/campaign-aero-im-b5.yaml` — analysis prompt confirms "Rust relay connector crate (aero-id pattern: lease/backoff/422→dead-terminal, client_credentials + claim contract, scope audit:event:write)"); in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md` (15-line gate summary; full v2 contract text + "37/37" test list are **out-of-repo [PROPOSED]**)
- **Status**: Requirements (verified evidence below)
- **Verification date**: 2026-08-06 (line numbers are as-of-verification anchors; drift is possible — the **file/symbol** is the stable grep anchor per AGENTS.md §0)

## 1. Evidence verification (every cited symbol checked against the repo)

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-storage/src/ai_usage.rs` — `MAX_LEASE_SECONDS=86_400`, `MAX_BACKOFF_SECONDS=300` (lines 14-15); claim ~475; settle ~497; requeue ~574; `ai_usage_backoff` ~683 | ✅ **Verified**. Constants exactly at lines 14-15. `claim_due` at 451: CTE `claimable` (`status='ready'`, `available_at <= now`, lease `IS NULL OR <= now`, `ORDER BY available_at, created_at, usage_id`, `FOR UPDATE SKIP LOCKED`, `LIMIT` clamped to `MAX_CLAIM=500`) → `UPDATE ... SET claim_token = gen_random_uuid(), lease_expires_at = $3, attempts = attempts + 1` at 475-476, `RETURNING` claim incl. token+lease. `settle` at 497: **one tx** — fenced re-read (`claim_token = $2 AND status = 'ready' AND lease_expires_at > clock_timestamp() FOR UPDATE`) → ledger `INSERT ... ON CONFLICT (usage_id) DO NOTHING` with payload-conflict `Protocol` error → ack `UPDATE ... SET status='completed', completed_at, claim_token=NULL, lease_expires_at=NULL, last_error=NULL` (rows_affected must be 1) → commit. Requeue = `mark_failed` at 576-599: re-park `available_at = now + ai_usage_backoff(attempts)`, clear token/lease, `last_error`, fenced on `claim_token AND attempts = $3 AND status='ready' AND lease_expires_at > clock_timestamp()`. **Doc comment (569-570): "There is deliberately no dead-letter: accounting rows remain retryable until they are durably recorded"** — the audit connector must *add* the dead terminal (E5). `ai_usage_backoff` at 683-688: exponential, `min(..., MAX_BACKOFF_SECONDS)`; `clamped_lease` at 692-694: `.clamp(1, MAX_LEASE_SECONDS)`. Also: `reserve` 160 / `finalize` 289 / `cancel` 362 / `recover_expired_reservations` 401. |
| E2 | `crates/aero-ai/src/usage.rs` — `UsageSink` trait reserve/finalize/cancel; `UsageContext::for_request`; `usage_id_for_job` | ✅ **Verified**. `UsageSink` at 56-81: `reserve(UsageEvent) -> UsageReserveOutcome {Acquired, InFlight, AlreadyFinalized(Option<UsageOutcome>)}` (57), `finalize(reservation, actual_micros, outcome)` (59), `cancel(reservation)` (66), plus default `persist` (reserve→finalize composition, ~70). `UsageContext::for_request(idempotency_key, actor, request_fingerprint, workspace)` at 124-141: deterministic `Uuid::new_v5(&USAGE_NAMESPACE, ...)` (namespace at 86) over `{actor, workspace, fingerprint, key}` — same inputs ⇒ same id; unit test `request_ids_repeat_only_for_the_same_actor_scope_payload_and_key` (362) proves key/actor/fingerprint sensitivity. `usage_id_for_job` (147) / `usage_id_for_moderation` (156): deterministic v5 with 1/2 domain tags; test `ids_are_namespaced_and_repeatable` (347). Production impl `PgUsageSink` (`crates/aero-server/src/ai_usage.rs:96-113`) maps `UsageEvent → UsageCharge` onto `AiUsageRepo::reserve/finalize/cancel`; wired at `bin/boot/services.rs:181`. **This is the stable-id reservation → finalize/cancel protocol the connector's event-id + fencing-token claim contract mirrors.** |
| E3 | `crates/aero-server/src/snaplink_commercial/http.rs` — `SCOPE_AUDIT` line 23, client_credentials grant line 233 | ✅ **Verified**. `const SCOPE_AUDIT: &str = "audit:event:write";` exactly at line 23. `request_token` at 233-254: `POST token_endpoint` with `basic_auth(client_id, client_secret)` + form `[("grant_type", "client_credentials"), ("scope", key.scope), ("resource", key.resource)]`; token cached per `(role, scope, resource)` with ttl-based `refresh_at` and 401 invalidation (`access_token` 182-232). `deliver` at 133-180: per-destination expected status (**audit ⇒ 202 ACCEPTED**), `Idempotency-Key` header (157) = `claim.idempotency_key`, audit receipt validated by `validate_audit_receipt` (330-351: `event_id`/`tenant_id` match claim, `accepted_at` present, `conflict=false`, `status ∈ {ledgered,indexed,archived}`); `validate_delivery_payload` (311-329: payload must not select tenant; audit `source_system` must equal trusted binding). **Note: v1 validates only bearer-token *shape* (`validate_token` 353-364) — claim validation (iss/aud/scope/sub) is NEW for the connector (R6).** |
| E4 | `migrations/0235_snaplink_commercial_control_plane.sql` — leased outbox DDL with `claim_token`/`lease_expires_at`/`last_error`, `UNIQUE(destination, idempotency_key)` | ✅ **Verified**. `snaplink_delivery_outbox` at 161-184: `idempotency_key TEXT NOT NULL` (168), `claim_token UUID` (173), `lease_expires_at TIMESTAMPTZ` (174), `delivered_at` (177), `last_error TEXT` (176), `attempts BIGINT CHECK >= 0`, claim-state CHECK (179-181: token+lease both NULL or both NOT NULL), **`UNIQUE (destination, idempotency_key)` (183)**. **Confirmed: no status enum and no dead terminal** — the only "done" signal is `delivered_at`; there is no row state from which delivery is never retried. Due index (186-188) `WHERE delivered_at IS NULL`. Rust claim twin in `crates/aero-storage/src/snaplink_commercial.rs`: `claim_due` 302 (same shape), `mark_delivered` 344, `mark_failed` 361, `reconcile_audit` 406. The connector's outbox repo must keep the same columns plus the dead state (B5-1 0239 owns the DDL). |
| E5 | `crates/aero-storage/src/ai_job.rs::AiJobRepo::fail` — existing dead-terminal precedent | ✅ **Verified**. Line 205: `UPDATE ai_jobs SET status = CASE WHEN attempts >= $3 THEN 'dead' ELSE 'queued' END, error = $2, finished_at = ... , scheduled_at = NOW() + LEAST(attempts,5)*5s WHERE id = $1`. The in-repo dead-terminal pattern: **attempts threshold selects terminal vs re-queue in one statement**; `defer` at 226 shows the non-consuming re-park. This is the precedent R5's `dead` transition clones (ai_usage's `mark_failed` deliberately lacks it, E1). |
| E6 | `docs/proposals/audit-contract-batch-aero-im.md` — [PROPOSED] exact 422/409/receipt-error → dead ≤1 semantics + claim contract text live in out-of-repo v2 docs | ✅ **Verified (as proposed)**. In-repo file is the 15-line gate summary; it confirms B5-2 = "新 crate（候选 `crates/aero-audit-connector`，无新第三方依赖）— §1.2 语义（lease > 2×timeout、退避 cap 300s、**422/409/回执错 → dead ≤1 次**、403 → dead = T-11 fail-closed）、cc + claim 校验（iss/aud/scope/sub）、usage relay 原地不动" and states the **"37/37" 测试清单 is out-of-repo**. The exact contract text is therefore **not verifiable in-repo**; this spec pins the in-repo normative reading (R5) and the in-repo drill subset (A3). |
| E7 | (supplementary) lease > 2×timeout precedent — `crates/aero-server/src/snaplink_commercial/config.rs` | ✅ **Verified**. Lines 246-251: `request_timeout` (env `AERO_SNAPLINK_REQUEST_TIMEOUT_SECS`, default 10s, range 1..120), `delivery_lease` (env `AERO_SNAPLINK_DELIVERY_LEASE_SECS`, default 30s, range 5..300), **boot-time `bail!` unless `delivery_lease > 2*request_timeout + 2s`**; lines 253-255 same invariant for task drain. The connector's config must clone this fail-loud invariant (R3/R9). |
| E8 | (supplementary) v1 relay is embedded in aero-server, not a crate; outbox has no dead-terminal | ✅ **Verified**. `crates/aero-server/src/lib.rs:106 pub mod snaplink_commercial;` — relay lives in aero-server (`{mod,http,runtime,config}.rs`), not a crate. `runtime.rs::deliver_claim` (233-281): Ok → `mark_delivered` (lease-lost ⇒ warn only, E1-style at-least-once); Err → `mark_failed` (re-park; fence lost ⇒ warn "lease expiry will reclaim"). **No code path transitions a row out of the retry set** — the dead terminal is genuinely absent (E4). |
| E9 | (supplementary) ai_usage db_tests — the "settle/requeue db_tests" A4 regression target | ✅ **Verified**. `crates/aero-storage/src/ai_usage/tests.rs`: `backoff_is_bounded_and_exponential` (22-30: 1→1s, 2→2s, 3→4s, `i32::MAX`→300s), `stable_reservation_and_settlement_are_exactly_once` (36), `finalized_outcome_replays_after_business_commit_failure` (102), `stale_claim_cannot_ack_after_reclaim` (141: **stale claim_token cannot ack after reclaim — the fencing test**), `ledger_insert_before_ack_is_deduplicated_on_retry` (182), `reused_id_with_different_payload_is_rejected`, `cancelled_provider_failure_can_reserve_a_fresh_retry`, `expired_ambiguous_reservation_is_conservatively_finalized`, `batch_insert_then_summary_rolls_up_per_kind`. Harness: `#[tokio::test] #[ignore = "requires live Postgres"]` + `DATABASE_URL` (tests.rs:8-14). |
| E10 | (supplementary) drill harness + outbox DDL home | ✅ **Verified**. `scripts/test-integration.sh` exists: throwaway-DB drill harness (per-direction DBs like `SNAPLINK_COMMERCIAL_INTEGRATION_DB="aero_snaplink_commercial_$$"`, `assert_disposable_db_name` guard, runs `--ignored` lib tests against the migrated main DB). `ai_usage_outbox` DDL lives in `migrations/0181_ai_usage_outbox.sql:19` (statuses `'reserved'/'ready'/'completed'`, `reservation_expires_at`, claim/lease/backoff columns) — the shape reference for the connector's own outbox contract. Workspace members listed in root `Cargo.toml:13-32` (new crate added there). |

## 2. Verified current state (the machinery this direction clones)

```
a) ai_usage relay state machine (PROVEN, db-tested)          crates/aero-storage/src/ai_usage.rs
   claim_due (451): status='ready' ∧ available_at ≤ now ∧ (lease NULL ∨ lease ≤ now)
                    ORDER BY available_at, created_at, usage_id  FOR UPDATE SKIP LOCKED
                    SET claim_token = gen_random_uuid(), lease_expires_at, attempts+1
   settle (497):    one tx: fenced re-read (claim_token ∧ lease unexpired) → ledger INSERT
                    ON CONFLICT DO NOTHING → ack UPDATE status='completed' (rows_affected=1)
   mark_failed (573): re-park available_at = now + ai_usage_backoff(attempts); clears token/lease;
                    fenced on claim_token ∧ attempts ∧ unexpired lease; last_error recorded
                    ❗ deliberately NO dead-letter (doc 569-570) — accounting rows always retryable
   ai_usage_backoff (683): 2^(attempts-1) sec, min(MAX_BACKOFF_SECONDS=300); lease clamp [1, 86_400]

b) UsageSink reserve/finalize/cancel protocol (PROVEN)        crates/aero-ai/src/usage.rs
   reserve(UsageEvent) → fenced stable-id reservation (UsageReservation{usage_id, token})
   finalize(reservation, actual_micros, outcome) / cancel(reservation)
   deterministic ids: UsageContext::for_request(key, actor, fingerprint, ws) → v5 UUID
                      usage_id_for_job(ulid) → v5 UUID    ⇒ stable id ⇒ idempotent replay
   production impl: PgUsageSink (crates/aero-server/src/ai_usage.rs:96) over AiUsageRepo

c) v1 Snaplink audit relay (EMBEDDED, no dead terminal)       crates/aero-server/src/snaplink_commercial/
   cc token: basic_auth + grant_type=client_credentials + scope="audit:event:write" (http.rs:23,233)
   deliver: POST audit_events_url, Idempotency-Key=event_id, expect 202, validate receipt (330-351)
   401 → invalidate+refresh; runtime: claim_due → for_each_concurrent → mark_delivered/mark_failed
   config: lease > 2×request_timeout + 2s bail (config.rs:246-251)
   outbox: snaplink_delivery_outbox (0235:161) — claim_token/lease_expires_at/last_error,
           UNIQUE(destination, idempotency_key), delivered_at only ⇒ NO dead state

d) dead-terminal precedent                                    crates/aero-storage/src/ai_job.rs:205
   status = CASE WHEN attempts >= max THEN 'dead' ELSE 'queued' END
```

**Gaps B5-2 closes** (all verified): (1) the relay is not a crate — B5-2 extracts/rebuilds it as `crates/aero-audit-connector`; (2) no dead terminal anywhere in the v1 outbox (E4/E8) — 422/409/receipt-error → dead ≤1 retry and 403 → dead (T-11) are new transitions; (3) claim validation (iss/aud/scope/sub) is new — v1 only shape-checks the bearer token (E3); (4) the state machine that *works* and is db-tested is `ai_usage`'s (E1) — the connector clones it rather than inventing one, and the stable-id idempotency contract comes from `UsageSink`/`UsageContext` (E2).

## 3. Scope

**In scope (this direction, B5-2)**:
- New workspace crate `crates/aero-audit-connector` (candidate name per proposal E6; zero new third-party deps — `reqwest`/`sqlx`/`tokio`/`time`/`uuid`/`serde` all already in the workspace, cf. `aero-ai/Cargo.toml` and `aero-server`'s snaplink module deps).
- The relay state machine cloned from `AiUsageRepo` (E1): claim (lease + rotated `gen_random_uuid()` token + `SKIP LOCKED`), settle (fenced ack, one tx), requeue (backoff capped at 300s), **plus the new dead terminal** cloned from `AiJobRepo::fail` (E5): 422/409/receipt-error → dead ≤1 retry; 403 → dead immediately (T-11 fail-closed).
- Outbox repo **trait seam** (claim_due/settle/requeue/mark_dead signatures mirroring `AiUsageRepo`) with an in-memory fake for unit tests; PG-backed impl binds the B5-1 0239 governance outbox (status 0/1/2/3, `event_id` 1:1 parity) — ownership boundary fixed at integration (R1).
- HTTP client cloned from the v1 relay (E3): `client_credentials` + `audit:event:write` scope, token cache with 401 invalidation, `Idempotency-Key` = event_id, 202 + receipt validation — **plus claim validation (iss/aud/scope/sub) before any POST** (R6).
- Config with the proven `lease > 2×timeout + 2s` boot-time bail and 300s backoff cap (R3/R9); boot wiring as a spawned task with the shared `CancellationToken` (AGENTS.md §4.3 server lifecycle) so the A3 drill is runnable.
- Unit tests A1/A2 (fake outbox + JWT claim validation), integration drill A3 pinned into `scripts/test-integration.sh`, regression A4.

**Out of scope (parallel directions / other modules — do not build here)**:
- `0239_audit_governance_outbox.sql` DDL (status 0/1/2/3, `class`, `priority`) + `audit_governance.rs` repo + enqueue/reconcile redirect → **B5-1 (aero-storage)**. The connector consumes it through the R1 trait; it does not own the DDL.
- `priority` ordering + anti-starvation in `claim_due` → **B5-3 (aero-storage)**.
- Scope-provisioning seam `aero-cli audit-provision-check` (grant `audit:event:write` only after relay works) → **B5-4 (aero-cli)**. The 403→dead fail-closed behavior (T-11) is this direction's; the provisioning *gate* is B5-4's.
- **`crates/aero-ai` code changes: none.** `usage.rs` is a read-only pattern source (E2). No edits to `crates/aero-storage/src/ai_usage.rs`, `crates/aero-server/src/ai_usage.rs` (`PgUsageSink`), or the snaplink **usage** destination path — "usage relay must stay untouched" (R8, A4).
- Retiring/redirecting the v1 embedded audit relay and the 0236-trigger → v2 cutover — B5-1/campaign decision (both tables may coexist during transition).
- Out-of-repo v2 contract documents and the IdP-side scope registry — cannot be built here; [PROPOSED] seams only.

## 4. Requirements

### R1 — New crate `aero-audit-connector` with an `OutboxRepo` trait seam
A new workspace member `crates/aero-audit-connector` (root `Cargo.toml` members list, E10) exposing:
- `OutboxRepo` (async trait): `claim_due(now, lease, limit) -> Vec<Claim>` (rows: stable `event_id`, `claim_token`, `lease_expires_at`, `attempts`, payload), `settle(event_id, claim_token) -> Result<bool, _>`, `requeue(event_id, claim_token, attempts, now, error) -> Result<bool, _>`, `mark_dead(event_id, claim_token, attempts, error) -> Result<bool, _>` — signatures mirroring `AiUsageRepo::{claim_due, settle, mark_failed}` (E1) plus the new dead transition.
- An in-memory fake implementing the trait (status/available_at/attempts/claim_token/lease_expires_at/last_error per row) for unit tests (A1). The PG-backed impl binds the **B5-1 0239 governance outbox** (status 0/1/2/3, `event_id` = `audit_events.id`, 1:1 parity) — whether the SQL lives in the connector or in B5-1's repo is fixed at integration, but the connector never writes the v1 `snaplink_delivery_outbox` table.
- A relay loop: `dispatch_batch` = claim ≤ batch-size due rows → concurrent delivery (bounded concurrency) → per row exactly one of settle / requeue / mark_dead; graceful drain on the shared `CancellationToken` (v1 `runtime.rs::dispatch_batch`/`deliver_claim` shape, E8; AGENTS.md §4.3).

### R2 — Claim contract: stable event-id + fencing token (UsageSink protocol mapping)
Each claim carries the **stable `event_id`** (the `audit_events.id` of the source row — B5-1 parity) and a **fresh fencing token** rotated per claim via `gen_random_uuid()` (clone `ai_usage.rs:475`). The idempotency protocol mirrors the `UsageSink` reserve/finalize/cancel shape (E2):
- `settle` succeeds **iff** `claim_token` matches AND lease is unexpired AND row is still claimable (clone `settle` fence, `ai_usage.rs:497-570`); a stale token from a reclaimed row must return `false` (proven by `stale_claim_cannot_ack_after_reclaim`, E9).
- `requeue`/`mark_dead` are fenced on `claim_token AND attempts` so a late re-park from a superseded attempt cannot overwrite a newer claim (clone `mark_failed` fence incl. `attempts = $3`, `ai_usage.rs:585-599`).
- Outbound `Idempotency-Key` = `event_id` (v1 precedent `http.rs:157`, `idempotency_key: event_id.into()` at 518) so a redelivery after a lost ack is idempotent at the sink — the durable-id determinism `UsageContext::for_request`/`usage_id_for_job` demonstrates (E2) maps 1:1 to this stable event-id contract.
- Reclaim after lease expiry yields a **new** token; the old token can never ack (E9 test is the model).

### R3 — Leased claim loop with `lease > 2×timeout` boot-time invariant
- Config load **fails loudly** (bail) unless `delivery_lease > 2 × request_timeout + 2s` — clone `snaplink_commercial/config.rs:246-251` (E7).
- Lease clamped to `[1s, MAX_LEASE_SECONDS=86_400]` (clone `clamped_lease`, `ai_usage.rs:692`).
- `claim_due` semantics: due = `available_at <= now` AND (`lease_expires_at IS NULL` OR `<= now`), ordered `(available_at, created_at, event_id)`, `FOR UPDATE SKIP LOCKED`, batch limit clamped (clone `ai_usage.rs:451-496`; `MAX_CLAIM=500` precedent).

### R4 — Backoff capped at 300s
`requeue` re-parks `available_at = now + backoff(attempts)` with `backoff = 2^(attempts-1)` seconds capped at `MAX_BACKOFF_SECONDS=300` (clone `ai_usage_backoff`, `ai_usage.rs:683-688`). Unit parity with the proven sequence (`backoff_is_bounded_and_exponential`, E9): 1→1s, 2→2s, 3→4s, … `i32::MAX`→300s.

### R5 — Dead terminal (new vs ai_usage; precedent `AiJobRepo::fail`)
- **Permanent error class** = HTTP 422, HTTP 409, or receipt-validation error (R7 receipt mismatch). Policy: first permanent-class failure → `requeue` (one retry, backoff per R4); second consecutive permanent-class failure (attempts ≥ 2) → `mark_dead` (**dead after ≤1 retry**; terminal, excluded from future claims, never re-parked; `last_error` recorded).
- **HTTP 403 → `mark_dead` immediately on the first attempt** — no requeue, no second delivery (T-11 fail-closed; mirrors the out-of-repo contract's T-11 gate per E6).
- Transient classes (transport error, timeout, 5xx, and 401-after-refresh) → `requeue` with backoff, never dead — the v1 retry-forever posture (E8) is preserved for transients.
- Dead transition implemented as a single `CASE`-style threshold statement cloned from `AiJobRepo::fail` (E5: `attempts >= max → 'dead'`), with the ai_usage doc-comment caveat inverted deliberately: **the audit outbox *does* have a dead-letter** because audit rows must not be silently re-sent forever, while ai_usage rows stay retryable (E1).

### R6 — client_credentials + claim validation (iss/aud/scope/sub) before any POST
- Token request clones v1: `basic_auth(client_id, client_secret)` + form `grant_type=client_credentials`, `scope=audit:event:write` (`SCOPE_AUDIT`, `http.rs:23`), `resource=audit_resource` (`http.rs:233-254`); token cached with ttl-based refresh and 401 invalidation (`http.rs:182-232`).
- **NEW (vs v1 shape-only `validate_token`, E3)**: before the first delivery POST with a token, validate its JWT claims against configured expectations: `iss` = configured issuer, `aud` contains configured audience, `scope` contains `audit:event:write`, `sub` = configured identity. Any mismatch ⇒ reject fail-closed (no POST with that token; token treated as unusable → refresh/reject). **Claim validation is a prerequisite of every delivery, not a one-time boot check** (A2 asserts POST counter stays 0 across all rejection cases).
- 401 on delivery ⇒ invalidate cached token, refresh once, retry; 403 ⇒ dead (R5). No POST may be sent with a token whose claims failed validation (T-11 adjacency).

### R7 — Delivery payload + durable receipt validation (clone v1)
- POST `audit_events_url` with `Bearer` + `Idempotency-Key: {event_id}` + payload; expected status **202 ACCEPTED** for audit (`http.rs:139-153`).
- Payload guard: no `tenant_id` selection; `source_system` must equal the trusted binding (clone `validate_delivery_payload`, `http.rs:311-329`).
- Receipt validation (clone `validate_audit_receipt`, `http.rs:330-351`): `event_id` matches the claim, `tenant_id` matches, `accepted_at` present, `conflict=false`, `status ∈ {ledgered, indexed, archived}`. **Receipt mismatch ⇒ permanent class ⇒ dead ≤1 retry (R5).**

### R8 — Usage relay untouched; no cross-talk
Zero changes to: `crates/aero-storage/src/ai_usage.rs` (repo + state machine), `crates/aero-ai/src/usage.rs` and all other `crates/aero-ai` sources, `crates/aero-server/src/ai_usage.rs` (`PgUsageSink`), and the snaplink **usage** destination path (claim/settle/reconcile_usage). The connector operates **only** on the audit outbox (R1); the usage relay's behavior is regression-pinned by A4. Enforced by review + A4's no-touch check, not by shared code (the connector deliberately does not import `AiUsageRepo`'s table).

### R9 — Config surface, fail-loud
Env-driven config (single-underscore `AERO_AUDIT_*` or `AERO_SNAPLINK_*`-style, per AGENTS.md §4.3 plain-env exceptions): token_endpoint, audit_events_url, audit_resource, audit client_id/secret, expected iss/aud/scope/sub, request_timeout, delivery_lease (R3 bail), batch_size, concurrency, drain budget. Missing/malformed config ⇒ boot error (fail-loud, v1 `config.rs` precedent E7). Backoff cap constant `MAX_BACKOFF_SECONDS=300` and `MAX_LEASE_SECONDS=86_400` exported for tests (A1 parity).

## 5. Acceptance checks (preserved from the direction, made testable)

All connector unit tests live in `crates/aero-audit-connector` (tokio tests, in-memory fake, **no PG**), mirroring the *assertion style* of the ai_usage db_tests (E9) — the fake reproduces their exact state transitions so the proven semantics are pinned without a database. A3 is the PG-backed integration drill.

### A1 — Connector unit tests against an in-memory fake outbox (mirroring ai_usage db_tests style)
**Setup**: fake `OutboxRepo` (R1) seeded with rows; deterministic `now` injected into claim/requeue.
**Act/Assert**:
1. **Lease expiry → row reclaimable** (mirrors `stale_claim_cannot_ack_after_reclaim`, E9): claim row with `lease=1s` at `t0` (token A); at `t0+2s` claim again → same row returned with a **new** token B (`token_a != token_b`); `settle(row, token_a)` → `false` (stale fence), `settle(row, token_b)` → `true`.
2. **Backoff sequence capped at 300s** (mirrors `backoff_is_bounded_and_exponential`, E9): `requeue` with attempts 1..9 → `available_at` offsets 1s, 2s, 4s, 8s, 16s, 32s, 64s, 128s, 256s, then 300s; `attempts = i32::MAX` → exactly 300s; `backoff(9) == backoff(i32::MAX) == 300s`.
3. **422/409/receipt-error → dead after ≤1 retry** (parameterized over the three permanent classes, R5): delivery attempt 1 returns the class error → row is `requeued` (not dead; `attempts=1`, `available_at` advanced by R4 backoff); delivery attempt 2 returns the same class error → row is `dead` (terminal; `claim_due` no longer returns it; `last_error` set). Exact assertion: `dead` after exactly 2 attempts for each class.
4. **403 → dead immediately (T-11 fail-closed)**: delivery attempt 1 returns 403 → row is `dead` directly (no requeue, exactly one attempt recorded, no second delivery; `claim_due` no longer returns it).
5. **Happy path**: successful delivery → `settle` returns `true`, row removed from claimable set (mirrors `stable_reservation_and_settlement_are_exactly_once`'s pending=0 assertion).

### A2 — Claim-validation tests: JWT iss/aud/scope/sub rejected before any POST; valid cc token accepted
**Setup**: stub token endpoint issues JWTs signed with test keys; stub audit endpoint counts POSTs; per-case token claim sets.
**Act**: connector requests token, then attempts delivery.
**Assert** (for each case): **no POST reaches the audit endpoint** — cases: (a) wrong `iss`; (b) wrong `aud` (audience not containing the configured value); (c) `scope` missing `audit:event:write`; (d) wrong `sub`. For the valid case (correct iss/aud/scope incl. `audit:event:write`/sub): token accepted and exactly the expected number of POSTs fire (delivery proceeds). Assertion mechanism: POST counter on the stub == 0 for all four rejection cases, ≥1 for the valid case — "rejected before any POST" is literally measured.

### A3 — 37/37 subset in-repo: pin the contract test list into `scripts/test-integration.sh`, then run the relay drill
**Part 1 (pin)**: the out-of-repo "37/37" contract test list is [PROPOSED] (E6) — when the contract text lands, its in-repo-verifiable subset is pinned into `scripts/test-integration.sh` as an explicit named test list (per-test `assert_disposable_db_name` guard + throwaway-DB harness precedent, E10). Until the list lands, this is a documented placeholder; the drill below is the in-repo subset.
**Part 2 (drill)**: in `scripts/test-integration.sh` style (E10): throwaway DB, full migration chain (incl. B5-1 0239), enqueue **N** audit rows (`N` = the pinned count; 37 when the list lands, otherwise the in-repo subset count) → run the connector binary against a stub audit endpoint (returns 202 + valid receipt) →
**Assert**: all N rows reach **delivered (status 2** per the B5-1 0239 status enum 0/1/2/3) — SQL: `COUNT(*) WHERE status = 2` == N — with **1:1 event_id parity** (P2): `SELECT event_id FROM outbox WHERE status = 2` set-equals `SELECT id FROM audit_events` for the batch (no duplicates, no orphans). Failure (any row stuck in 0/1/3, or parity mismatch) fails the script.

### A4 — Regression: ai_usage relay behavior unchanged
**Assert**:
1. The existing ai_usage db_tests stay green, run via the standard harness (`cargo test --workspace --lib -- --ignored` with `DATABASE_URL`, as `test-integration.sh` does): `stable_reservation_and_settlement_are_exactly_once`, `finalized_outcome_replays_after_business_commit_failure`, `stale_claim_cannot_ack_after_reclaim`, `ledger_insert_before_ack_is_deduplicated_on_retry`, `reused_id_with_different_payload_is_rejected`, `cancelled_provider_failure_can_reserve_a_fresh_retry`, `expired_ambiguous_reservation_is_conservatively_finalized`, `batch_insert_then_summary_rolls_up_per_kind`, `backoff_is_bounded_and_exponential` (E9) — **and** the v1 snaplink unit tests (`hard_zero_is_not_treated_as_unlimited`, `unlimited_requires_zero_numeric_limits`, `audit_delivery_requires_a_matching_durable_receipt`, http.rs test module).
2. **No-touch guard**: `git diff` over `crates/aero-storage/src/ai_usage.rs`, `crates/aero-ai/src/usage.rs`, `crates/aero-server/src/ai_usage.rs`, and the snaplink usage destination path is empty (R8). (Boot wiring that *adds* the connector task is the only permitted aero-server delta; the usage relay code paths are not modified.)

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| A1 fake-outbox state machine (lease reclaim, backoff cap, dead ≤1, 403→dead, happy settle) | `crates/aero-audit-connector` unit tests (in-memory fake) | `cargo test -p aero-audit-connector`, no DB, tokio |
| A2 claim validation (iss/aud/scope/sub) | `crates/aero-audit-connector` unit tests (stub token endpoint, POST counter) | same |
| A3 relay drill (N rows → status 2, event_id parity) + pinned contract list | `scripts/test-integration.sh` | integration, throwaway DB + stub audit endpoint + connector binary |
| A4 regression (ai_usage + snaplink tests green, no-touch diff) | existing `crates/aero-storage/src/ai_usage/tests.rs` + `snaplink_commercial/http.rs` tests; review gate | `cargo test --workspace --lib -- --ignored` via test-integration.sh |

## 7. Risks / [PROPOSED] items

- **Exact 422/409 semantics and claim-contract text live in out-of-repo v2 docs** (E6). This spec pins the in-repo normative reading: permanent-class ⇒ requeue once ⇒ dead (≤2 total attempts); 403 ⇒ dead on attempt 1. If the contract text contradicts this reading, only R5's thresholds change (A1's parameterized test is the enforcement point).
- **"37/37" test list is out-of-repo** (E6). A3 pins the *in-repo subset* (the drill) now and the full list when the contract text lands; the drill itself is fully testable in-repo.
- **Status enum values (0/1/2/3, "status 2" = delivered) are 0239/B5-1 DDL's**, not this direction's. A3 asserts against B5-1's status constants, not hardcoded numbers; the drill cannot run until 0239 + the PG repo land (sequencing below).
- **Dead-terminal ownership**: the connector's `mark_dead` writes through B5-1's outbox (or owns the SQL — fixed at integration, R1). During the v1→v2 cutover both `snaplink_delivery_outbox` (no dead state) and the governance outbox (dead state) coexist; the 0236-trigger redirect is B5-1's, and the connector must never claim v1 rows.
- **Token shape**: v1 `validate_token` only shape-checks the bearer (E3); if the IdP issues opaque (non-JWT) tokens, claim validation cannot run — the connector must fail closed (no POST) rather than skip validation. Decision noted; A2 assumes a JWT-issuing stub.
- **aero-ai is read-only**: `usage.rs` is the pattern source (E2); any temptation to "share" the trait by editing aero-ai is out of scope — the connector defines its own claim contract mirroring it (R2), and A4 guards the diff.
- **Cloning vs sharing the state machine**: A4 forbids touching `ai_usage.rs`, so the connector deliberately re-implements the proven SQL shape in its own outbox (R1) — duplicated code is the price of "usage relay must stay untouched"; the duplication is pinned by A1 mirroring E9's assertions.

## 8. Sequencing

1. **This direction, phase 1 (no PG, no B5-1 dependency)**: connector crate skeleton + `OutboxRepo` trait + in-memory fake + claim/settle/requeue/dead policy + HTTP client (cc token, claim validation, receipt validation) + config with the lease/backoff bails + **A1 + A2 green**. All runnable with the fake outbox and stub endpoints.
2. **B5-1 lands (0239 DDL + repo + status enum)**: bind the PG impl to the governance outbox (R1 ownership decision) → **A3 drill** added to `scripts/test-integration.sh` (part 2; part 1 placeholder until contract text lands).
3. **B5-3** (priority claim) and **B5-4** (provisioning seam) proceed in parallel — neither changes the connector's state machine; B5-4's gate consumes this direction's 403→dead fail-closed behavior (T-11).
4. **A4** is a standing gate for every step: `--ignored` suite green + no-touch diff on the usage relay.
