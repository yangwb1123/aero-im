# Requirements Spec — B5-2: `crates/aero-audit-connector` built by porting the three proven lease/backoff/dead-terminal machines (snaplink relay · webhook_delivery · stream_go_live_outbox)

- **Module (analysis root)**: `crates/aero-live-rtmp/src` — the RTMP ingest module whose lifecycle (publish/refuse/end) is the first in-tx audit source under B5-1 (out of scope here); the connector crate is the delivery machine that drains B5-1's governance outbox.
- **Direction**: "Build B5-2's aero-audit-connector crate by porting the repo's three proven lease/backoff/dead-terminal machines instead of inventing a new state machine" (value 8 / risk_reduction 9 / effort 7 / confidence 8)
- **Source analysis**: `docs/auto/analyses/crates-aero-live-rtmp-src-9b434347.json` (direction #2)
- **Campaign**: `aero-im-b5-outbox-relay`; in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md` (15-line gate summary; full v2 contract text + "37/37" list are out-of-repo, [PROPOSED])
- **Status**: Requirements (all cited evidence verified against the repo; the working-tree port's tests re-run and counted)
- **Verification date**: 2026-08-07. Line numbers are as-of-verification anchors and may drift — the **file/symbol** is the stable grep anchor (AGENTS.md §0).

## 1. Evidence verification (every cited file/symbol checked against the repo)

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-server/src/snaplink_commercial/http.rs:23` (`SCOPE_AUDIT = "audit:event:write"`), http.rs access_token/claim machinery, runtime.rs:221-277 (delivery lease, lease-loss re-park, fencing) | ✅ **Verified**. `const SCOPE_AUDIT: &str = "audit:event:write";` exactly at :23. `request_token` :233-254: `basic_auth(client_id, client_secret)` + form `grant_type=client_credentials`, `scope`, `resource`; token cached per `(role, scope, resource)` with ttl-derived `refresh_at` and 401 invalidation (`access_token` :182-232 — lock never held across network I/O). `deliver` :133-180: audit destination expects **202 ACCEPTED** (`:144-153`), `Idempotency-Key` = `claim.idempotency_key` (:157), receipt validated by `validate_audit_receipt` :330-351 (event_id/tenant_id match claim, `accepted_at` present, `conflict=false`, status ∈ {ledgered,indexed,archived}). `validate_token` :353-364 is **shape-only** — no JWT claim validation (the direction's "claim validation is new" reading confirmed). `runtime.rs:221-277`: `dispatch_batch` claims with `self.config.delivery_lease` (:219-223); `deliver_claim` :247-281 — Ok → `mark_delivered`, lease lost ⇒ warn "delivery lost its lease before acknowledgement" (:255-260); Err → `mark_failed` re-park, fence lost ⇒ warn "explicit re-park lost its fence; lease expiry will reclaim" (:267-279). |
| E2 | `crates/aero-storage/src/webhook_delivery.rs:9-20,35-37,76-79,105` (pending→delivered\|failed→dead lifecycle, MAX_ATTEMPTS, `is_dead_at` pure fn) | ✅ **Verified**. :8-10 doc: `status` walks `pending → delivered | failed → dead`; a random `claim_token` owns every pending generation. :35-38 `pub const MAX_ATTEMPTS: i32 = 6` ("once `attempts` reaches this, a further failure is terminal rather than retried"). :60-67 `backoff_delay(attempts)` pure total fn (2^n base, clamped to `MAX_DELAY_SECS=3600`). :76-81 `pub fn is_dead_at(attempts: i32) -> bool { attempts >= MAX_ATTEMPTS }` — **clock-free pure cap predicate, unit-tested without a DB** (module doc :21-26 states this seam explicitly). :105-106 row `status` field. Fencing: `begin_attempt` :180-202 (charges attempts only immediately before the POST, `WHERE status='pending' AND claim_token=$2 AND attempts < MAX_ATTEMPTS`), `mark_failed_with_backoff` :236-277 (dead branch vs `next_attempt_at` backoff branch — single `is_dead_at(attempts)` decision), `claim_due` :276-343 (stale-pending recovery + **abandoned final attempt → dead** pre-pass :286-300; `FOR UPDATE SKIP LOCKED`; token rotation). |
| E3 | `crates/aero-storage/src/stream_go_live_outbox.rs:20,54,134,157,290-303` (attempts, claim_token, retry_delay(attempts), available_at) | ✅ **Verified**. :14-16 `MAX_CLAIM=500`, `MAX_LEASE_SECONDS=86_400`, **`MAX_BACKOFF_SECONDS=300`**; :20 `COLUMNS` includes `attempts, claim_token, available_at, claimed_at, completed_at, last_error`; :54 `attempts: i32` field; :134 doc "`attempts` counts relay claims"; :157 claim CTE `attempts = outbox.attempts + 1` with `FOR UPDATE SKIP LOCKED` (:147) and rotated `claim_token = gen_random_uuid()` (:156); :286-311 `mark_failed` — fenced re-park: `available_at = now + retry_delay(attempts)` (:294) computed **in Rust** from the injected `now`, `WHERE id AND claim_token AND attempts = $3 AND completed_at IS NULL` (:297-304). :357-365 `clamp_lease` + `retry_delay` (exponential `2^(attempts-1)` capped at 300s); :384-391 pure-fn unit test (`retry_delay(1)=1s, retry_delay(2)=2s, retry_delay(99)=300s`). |
| E4 | `crates/aero-storage/src/consumer_event_receipt.rs` (external-side-effect receipt leases) | ✅ **Verified**. `ConsumerEventClaim::{Claimed{attempts},Completed,Busy}` :17-41. `claim` :43-99: `INSERT … ON CONFLICT (consumer,event_id) DO UPDATE … WHERE state='processing' AND lease_expires_at <= $4 RETURNING attempts` — **attempts is the fencing token**; an expired lease is reclaimable; a live lease answers `Busy`; a completed receipt answers `Completed`. `renew` :101-132 and `complete` :134-170 fence on `attempts = $3 AND lease_expires_at > clock_timestamp()` (double fence: attempt generation + unexpired lease); `release` :198-220 fenced re-park. `clamped_lease` :240-242 `[1s, 86_400s]`. This is the lease+fencing model the connector's claim contract mirrors. |
| E5 | `crates/aero-server/src/bin/boot/background.rs` (golive outbox relay wiring; golive_bot per AGENTS.md §2) | ✅ **Verified**. :99-140: `AERO__SERVER__STREAM_LIVE_OUTBOX_POLL_MS`-gated spawn — `tracker.spawn` + `tokio::time::interval` + `MissedTickBehavior::Skip` + `tokio::select!` biased on the shared `CancellationToken` (cancel ⇒ break, no drain here) + per-tick `dispatch_batch(&s, 100)` with warn-not-panic on error + enabled/disabled info logs. This is the spawn template for the connector's boot wiring. |

**Supplementary verified facts**:

| # | Fact | Verification result |
|---|---|---|
| S1 | snaplink storage claim/lease machine (the third proven piece the direction names as "usage relay") | ✅ `crates/aero-storage/src/snaplink_commercial.rs`: `SnaplinkDeliveryClaim` :77-93 (delivery_id/destination/tenant/client/source/idempotency_key/payload/attempts/claim_token/lease_expires_at); `claim_due` :302-343 (claimable CTE: `delivered_at IS NULL AND available_at <= now AND (lease IS NULL OR lease <= now)`, `ORDER BY available_at, created_at, delivery_id`, `FOR UPDATE SKIP LOCKED`, `LIMIT` ≤ `MAX_CLAIM=500`; `SET claim_token = gen_random_uuid(), lease_expires_at = $3, attempts = attempts + 1` :327-328); `mark_delivered` :344-359 (fenced `claim_token = $2 AND lease_expires_at > clock_timestamp()`); `mark_failed` :361-383 (re-park `available_at = now + commercial_delivery_backoff(attempts)`, same fence); `commercial_delivery_backoff` :710-719 (deterministic per-delivery jitter, cap `MAX_BACKOFF_SECS=300` :18). **No status column, no dead terminal** — `delivered_at` is the only terminal. |
| S2 | `lease > 2×timeout` already exists as a boot-time bail in v1 | ✅ `config.rs:246-251`: `delivery_lease <= request_timeout.saturating_mul(2) + 2s ⇒ bail!`; same invariant for task drain :253-255. Defaults: request_timeout 10s (1..120), delivery_lease 30s (5..300). |
| S3 | RTMP module relationship (module under analysis) | ✅ `crates/aero-live-rtmp/src/lib.rs`: `process_results` :410 → `repo.mark_live(stream.id, &hls_url)` :483 on `PublishStreamRequested` (the in-tx outbox CTE source B5-1 generalizes); `repo.mark_ended` :362/:525 (bare UPDATE — B5-1's gap, out of scope here); `validate_stream_key` :123 + `StreamKeyRejection` :93 (fail-closed shape-check precedent, cited by the sibling B5-3 direction). The connector is delivery-only; it never touches this module. |
| S4 | Working tree already contains an uncommitted `crates/aero-audit-connector/` port (batch's earlier implement stage) | ✅ Exists as untracked files; root `Cargo.toml:29` member + :77 dependency; boot wiring `crates/aero-server/src/bin/main.rs:251-259` (RelayConfig::from_env → PgOutboxRepo → AuditClient → AuditRelay spawn); `scripts/test-integration.sh:33` throwaway-DB slot. **`cargo test -p aero-audit-connector` re-run this session: 24/24 pass + 1 `--ignored` PG test** (`pg::tests::concurrent_double_claim_across_two_sessions_is_impossible`). Deps: tokio/tokio-util/async-trait/futures/reqwest/sqlx/serde/serde_json/thiserror/anyhow/tracing/time/uuid + `base64 = "0.22"` — base64 0.22 already in the workspace lock (aero-server/aero-live-srt), i.e. **zero new third-party deps** (Cargo.lock :857-859). |
| S5 | no-touch guard currently holds | ✅ `git diff --stat` over `crates/aero-server/src/snaplink_commercial/` and `crates/aero-storage/src/snaplink_commercial.rs` is **empty**; the only server-side delta is the boot wiring in `main.rs`. |

### 1.1 Corrections to the direction's claims (evidence-backed)

- **Proposed claim "aero-audit-connector being a new crate does not exist yet" is now FALSE in the working tree.** It was true at analysis time; since then the batch's implement stage left an uncommitted port at `crates/aero-audit-connector/` (S4). This spec therefore pins the *semantics* (the direction's acceptance set) and treats the working-tree port as the port-in-progress to be verified against them — not as something to be invented from scratch.
- **"lease > 2×timeout" already exists** in v1 (`config.rs:246-251`, S2); the connector requirement is to clone the bail, not invent it.
- **Backoff cap 300s already exists in two storage machines** (`stream_go_live_outbox.rs:16`, `snaplink_commercial.rs:18`); what is new is the **pure-fn shape** the direction explicitly requires ("keeping dead-at and backoff as clock-free pure functions like webhook_delivery.rs") — `webhook_delivery.rs` is the only one of the three that already has both as pure fns (`backoff_delay` :60-67, `is_dead_at` :76-81).
- **Genuine gaps the direction closes** (all verified): ① relay is embedded in aero-server, not a crate (S1/S5); ② no dead terminal anywhere in the v1 outbox (S1); ③ claim validation (iss/aud/scope/sub) is absent — v1 shape-checks only (E1); ④ 403 is today treated as a retryable failure, not a fail-closed terminal (E1 deliver arm).

## 2. Verified current state (the three machines this direction ports)

```
a) snaplink commercial relay (v1, embedded in aero-server)   snaplink_commercial/{http,runtime,config}.rs
   cc token: basic_auth + grant_type=client_credentials + scope="audit:event:write" (http.rs:23,233-254)
   token cache with ttl refresh_at + 401 invalidation (http.rs:182-232)
   deliver: POST audit_events_url, Idempotency-Key=idempotency_key, expect 202 + receipt (http.rs:133-180,330-351)
   relay loop: claim_due(delivery_lease) → for_each_concurrent → mark_delivered/mark_failed (runtime.rs:215-281)
   config: delivery_lease > 2×request_timeout + 2s boot bail (config.rs:246-251)
   storage: claim_token rotation + lease fence + attempts+1 + 300s-capped backoff (snaplink_commercial.rs:302-383,710-719)
   ❗ no status enum, no dead terminal — delivered_at is the only terminal (S1)

b) webhook_delivery.rs  (PURE-FN SHAPE — the template for dead-at/backoff)   crates/aero-storage/src/webhook_delivery.rs
   MAX_ATTEMPTS=6 (:38); is_dead_at(attempts) pure fn (:76-81); backoff_delay/next_attempt_at pure fns (:60-74)
   lifecycle pending → delivered | failed → dead (:8-10); claim_token owns each pending generation (:9-10)
   begin_attempt charges attempts only at the POST (fenced, :180-202); mark_failed_with_backoff branches
   dead-vs-failed on is_dead_at (:236-277); claim_due stale recovery + abandoned final attempt → dead (:276-343)

c) stream_go_live_outbox.rs  (LEASED RELAY — the wiring template)   crates/aero-storage/src/stream_go_live_outbox.rs
   attempts/claim_token/available_at/claimed_at/completed_at/last_error (:20); MAX_BACKOFF_SECONDS=300 (:16)
   claim_due: SKIP LOCKED, attempts+1, token rotation (:84-163); mark_failed: fenced re-park,
   available_at = now + retry_delay(attempts) computed in Rust (:286-311); retry_delay pure fn, unit-tested (:360-365,384-391)
   boot wiring: bin/boot/background.rs:99-140 (poll interval + CancellationToken + warn-not-panic)

d) consumer_event_receipt.rs  (RECEIPT LEASE — attempts-as-fence model)   crates/aero-storage/src/consumer_event_receipt.rs
   claim: ON CONFLICT … WHERE lease_expires_at <= now RETURNING attempts (:43-99)
   renew/complete/release: double fence attempts = $3 AND lease_expires_at > clock_timestamp() (:101-220)
```

**Working-tree port state** (S4): `crates/aero-audit-connector/` = `outbox.rs` (async `OutboxRepo` trait: `claim_due(lease,limit)`, `settle`, `requeue`, `mark_dead` — with a deliberate **single clock domain**: the repo owns time, no caller-supplied `now`, so |app↔DB skew| ≥ lease cannot livelock claim→POST→fence-fail), `fake.rs` (in-memory fake, injectable pinned clock, status Ready/Delivered/Dead), `pg.rs` (`PgOutboxRepo` over `audit_governance_outbox` status 0/1/2/3 with a minimal 0239-equivalent DDL seam + the two-session concurrent-claim PG test), `client.rs` (cc token + `validate_token_claims` iss/aud/scope/sub fail-closed before POST + 403→`Forbidden`, 422/409/receipt→`Permanent`, else `Transient`), `relay.rs` (`audit_backoff` pure fn 1→1s … 10→300s, `clamped_lease`, `deliver_claim` arms, poll loop + shutdown drain), `config.rs` (`check_lease_invariant` cloned from v1), `stub.rs` (token/events stub with POST counter), drill bins (`aero-audit-t11-drill`, `aero-audit-relay-drill`, `aero-audit-priority-drill`).

## 3. Scope

**In scope (this direction, B5-2)**:
- The `aero-audit-connector` crate (candidate name per contract; already a workspace member in the working tree) with **zero new third-party deps** (S4: base64 0.22 already pinned).
- The relay state machine ported from the three machines: claim (lease + rotated token + SKIP LOCKED — E3/E1), settle (fenced ack — E1), requeue (backoff capped 300s — E3), dead terminal (E2 `is_dead_at` shape; 422/409/receipt-error → requeue on attempt 1, dead on attempt 2; **403 → dead on attempt 1, T-11 fail-closed**).
- `OutboxRepo` trait seam + in-memory fake (unit tests, no DB) + PG impl bound to B5-1's 0239 governance outbox (status 0/1/2/3, `event_id` 1:1 parity with `audit_events.id`); the connector **never writes** the v1 `snaplink_delivery_outbox` table.
- HTTP client cloned from v1 (cc + `audit:event:write`, token cache + 401 refresh-once, `Idempotency-Key` = event_id, 202 + durable-receipt validation) **plus claim validation (iss/aud/scope/sub) before every POST**.
- Config with the proven `lease > 2×timeout + 2s` bail and 300s backoff cap; boot wiring as a spawned task on the shared `CancellationToken` (background.rs :99-140 template).
- Dead-at and backoff as **clock-free pure functions** (webhook_delivery.rs :60-81 shape), unit-tested without clock or DB.

**Out of scope (parallel directions / other modules — do not build here)**:
- `0239_audit_governance_outbox.sql` DDL + `audit_governance.rs` repo + enqueue/reconcile redirect + RTMP lifecycle in-tx audit writes → **B5-1 (aero-storage / aero-live-rtmp)**. The connector consumes the outbox through the R1 trait; it does not own the DDL or the RTMP call sites.
- `priority` ordering / anti-starvation in `claim_due` → **B5-3**. The drill bin `aero-audit-priority-drill` is a seam, not this direction's deliverable.
- Scope-provisioning gate (`aero-cli audit-provision-check`, grant `audit:event:write` only after relay works) → **B5-4**. The 403→dead fail-closed *behavior* (T-11) is this direction's; the provisioning *gate* is B5-4's.
- **The snaplink usage relay: untouched** (S5 no-touch guard; A7 regression). Zero changes to `snaplink_commercial/{mod,http,runtime,config}.rs`, `crates/aero-storage/src/snaplink_commercial.rs`, or the 0235 trigger/outbox.
- Retiring/redirecting the v1 embedded audit relay → B5-1/campaign cutover decision; both tables may coexist during transition.
- Out-of-repo v2 contract documents and the IdP-side scope registry — [PROPOSED] seams only.

## 4. Requirements

### R1 — `aero-audit-connector` crate + `OutboxRepo` trait seam
New workspace member (already added in the working tree, S4) exposing:
- `OutboxRepo` (async trait, `Send + Sync`): `claim_due(lease, limit) -> Vec<Claim>`, `settle(event_id, claim_token) -> Result<bool>`, `requeue(event_id, claim_token, attempts, error) -> Result<bool>`, `mark_dead(event_id, claim_token, attempts, error) -> Result<bool>` — signatures mirroring the three machines (E1 `mark_failed` fence incl. `attempts`, E3 `mark_failed` shape, E2 `mark_dead`).
- `Claim` carries the **stable `event_id`** (the `audit_events.id` of the source row — B5-1 parity), `claim_token`, `lease_expires_at`, `attempts`, `payload`.
- In-memory fake implementing the trait for unit tests (A1); PG impl binds B5-1's 0239 governance outbox (status 0/1/2/3, `event_id` 1:1) and never writes the v1 table (S1).
- Relay loop: `dispatch_batch` = claim ≤ batch due rows → concurrent delivery (bounded concurrency) → per row exactly one of settle/requeue/mark_dead; poll loop + graceful drain on the shared `CancellationToken` (background.rs :99-140 template; runtime.rs :215-232 shape).

### R2 — Claim contract: stable event-id + fencing token
- `claim_token` rotated per claim via `gen_random_uuid()` (E1 :327-328 / E3 :156); `attempts` incremented only by claims that begin delivery work (E3 :157, E2 `begin_attempt` charges only at the POST).
- `settle`/`requeue`/`mark_dead` all fenced on `claim_token AND attempts` (and, where the schema has it, an unexpired lease) — a stale worker from a superseded claim receives `false`, never an error (E1 :352-354/:373-375, E3 :297-304, E4 :134-170).
- Outbound `Idempotency-Key` = `event_id` (E1 :157) so a redelivery after a lost ack is idempotent at the sink.
- **Single clock domain**: the repo owns time exclusively (`clock_timestamp()` in PG; injectable pinned clock in the fake) — no caller-supplied `now` enters the fence path, so |app↔DB skew| ≥ lease cannot livelock claim→POST→fence-fail (working-tree `outbox.rs` doc; pinned by A1-6).

### R3 — Leased claim loop with `lease > 2×timeout` boot-time invariant
- Config load **fails loudly** unless `delivery_lease > 2 × request_timeout + 2s` (clone `config.rs:246-251`; working-tree `check_lease_invariant`); task-drain variant :253-255 cloned too.
- Lease clamped `[1s, MAX_LEASE_SECONDS=86_400]` (E3 :357, E4 :240).
- `claim_due` semantics: due = `available_at <= now` AND (lease NULL or `<= now`), ordered `(available_at, created_at, event_id)`, `FOR UPDATE SKIP LOCKED`, batch limit clamped (E1 :302-343 / E3 :84-163; `MAX_CLAIM=500` precedent).

### R4 — Backoff capped at 300s, as a clock-free pure function
- `requeue` re-parks `available_at = repo_now + audit_backoff(attempts)` with `audit_backoff(attempts) = 2^(attempts-1)` seconds capped at `MAX_BACKOFF_SECONDS=300` — the proven sequence (E3 :360-365, S1 :710-719): `1→1s, 2→2s, 3→4s, …, 9→256s, 10..=i32::MAX→300s`.
- `audit_backoff` is a **pure, total function** in the `webhook_delivery.rs::backoff_delay` shape (E2 :60-67) — no clock, no DB — and unit-tested as such (A4).

### R5 — Dead terminal via a clock-free `is_dead_at` pure function
- **Permanent error class** = HTTP 422, HTTP 409, or receipt-validation error (R7 mismatch). Policy: attempt 1 → `requeue` (one retry, R4 backoff); attempt ≥ 2 → `mark_dead` (**dead after ≤1 retry**; terminal, excluded from future claims; `last_error` recorded).
- **HTTP 403 → `mark_dead` on the first attempt** — no requeue, no second delivery (T-11 fail-closed; E1's v1 treats 403 as a retryable non-202 — this is the behavior change).
- Transient classes (transport error, timeout, 5xx, unspecified 4xx, claim-validation drift, 401-after-refresh) → `requeue` with backoff, **never dead** (v1 retry-forever posture preserved, E1 :267-279).
- The threshold decision lives in a **pure predicate `is_dead_at(attempts) -> bool`** in the `webhook_delivery.rs::is_dead_at` shape (E2 :76-81), unit-tested without clock/DB; `mark_failed`-style SQL branches on it (E2 :236-277 pattern).
- Deliberate inversion of ai_usage's "no dead-letter" doc: the audit outbox *does* have a dead-letter, because audit rows must not be silently re-sent forever (S1).

### R6 — client_credentials + claim validation (iss/aud/scope/sub) before any POST
- Token request clones v1: `basic_auth` + form `grant_type=client_credentials`, `scope=audit:event:write` (`SCOPE_AUDIT`, E1 :23), `resource`; token cached with ttl-based refresh and 401 invalidation (E1 :182-232).
- **Before the first delivery POST with a token, validate its JWT claims** against configured expectations: `iss` = configured issuer, `aud` contains configured audience, `scope` contains `audit:event:write`, `sub` = configured identity. Any mismatch ⇒ **fail-closed: no POST with that token** (treated as unusable → refresh/reject). This is a per-delivery prerequisite, not a one-time boot check (A5 asserts the POST counter stays 0 across all rejection cases).
- 401 on delivery ⇒ invalidate cached token, refresh once, **re-validate claims before the retry POST**; 403 ⇒ dead (R5). Opaque non-JWT tokens cannot be claim-validated ⇒ fail closed (no POST).

### R7 — Delivery payload + durable receipt validation (clone v1)
- POST `audit_events_url` with `Bearer` + `Idempotency-Key: {event_id}` + payload; expected **202 ACCEPTED** (E1 :144-153).
- Payload guard: no `tenant_id` selection; audit `source_system` must equal the trusted binding (E1 `validate_delivery_payload` :311-329).
- Receipt validation (E1 `validate_audit_receipt` :330-351): `event_id` matches the claim, `tenant_id` matches, `accepted_at` present, `conflict=false`, `status ∈ {ledgered,indexed,archived}`. Receipt mismatch ⇒ permanent class ⇒ dead ≤1 retry (R5).

### R8 — Usage relay untouched; no cross-talk
Zero changes to: `crates/aero-server/src/snaplink_commercial/{mod,http,runtime,config}.rs`, `crates/aero-storage/src/snaplink_commercial.rs`, and the 0235 `snaplink_delivery_outbox` table/trigger. The connector operates only on the audit outbox (R1) and never claims v1 rows. Enforced by A7's no-touch diff guard.

### R9 — Config surface, fail-loud
Env-driven config in the v1 style (single-underscore `AERO_AUDIT_*` plain envs per AGENTS.md §4.3): token_endpoint, audit_events_url, audit_resource, audit client_id/secret, expected iss/aud/scope/sub, source_system, request_timeout, delivery_lease (R3 bail), batch_size, concurrency, poll_interval, drain budget. Missing/malformed config ⇒ boot error (fail-loud, v1 precedent E7/S2). Exported constants `MAX_BACKOFF_SECONDS=300`, `MAX_LEASE_SECONDS=86_400`, `MAX_CLAIM=500` for tests.

## 5. Acceptance checks (preserved from the direction, made testable)

All unit tests run in `crates/aero-audit-connector` against the in-memory fake + stub sink (**no PG**); the PG-gated tests run via the `--ignored` harness. Each check maps to a concrete test symbol that exists in the working-tree port (S4) or is pinned as a required addition.

### A1 (T-11) — 403 → dead ≤ 1 attempt with no retry (fail-closed) unit test
**Test**: `tests/state_machine.rs::forbidden_dead_on_first_attempt` (stub sink returns 403).
**Assert**: after one `dispatch_batch` — row status `Dead` (not requeued); `attempts == 1` ("≤1 attempt" literal); `stub.posts() == 1` (exactly one delivery POST, no second delivery); `claim_due` never returns the row again; `last_error` records the 403 reason.

### A2 — 422 → dead ≤ 1
**Test**: `tests/state_machine.rs::permanent_error_dead_after_exactly_two_attempts` (first behavior arm: `events_status: 422`).
**Assert** (per arm): attempt 1 → status `Ready` (requeued, not dead), `attempts == 1`, `available_at == t0 + backoff(1)` = `t0 + 1s` on the pinned fake clock, `last_error` contains the permanent class; attempt 2 (after `make_due_now`) → status `Dead`, `attempts == 2`; `claim_due` returns nothing thereafter.

### A3 — 409 / receipt-error → dead
**Test**: same `permanent_error_dead_after_exactly_two_attempts`, remaining arms (`events_status: 409`; `events_status: 202, receipt_valid: false`).
**Assert**: identical to A2 for each arm — **dead after exactly 2 attempts** (requeue once, then terminal). The parameterized loop is the enforcement point: all three permanent classes share one assertion path.

### A4 — Lease fencing test: two concurrent claimants on one row, exactly one wins, loser observes claim_token mismatch; backoff monotonic with cap 300s (pure-fn, no clock/DB)
**Fencing (two parts)**:
- Same-instant double-claim on one row: two `claim_due` calls at the same repo-clock instant with a lease that has not expired — exactly **one** returns the row with a fresh token; the second sees the row filtered (lease unexpired) and returns no claim (or, after expiry, a rotated token). Loser-observation: `tests/state_machine.rs::stale_token_cannot_ack_after_reclaim` asserts `token_b != token_a`, `settle(id, token_a) == false` (loser's token can never acknowledge), `settle(id, token_b) == true`.
- True cross-session concurrency: `src/pg.rs::concurrent_double_claim_across_two_sessions_is_impossible` (`--ignored`, live PG): 50 due rows, two independent sessions × `LIMIT 25` behind a barrier — `FOR UPDATE SKIP LOCKED` hands each session a disjoint 25 with `attempts == 1` everywhere; zero double-claims. (Sequencing: runs when a throwaway PG is available, per the `test-integration.sh` harness.)
- `tests/state_machine.rs::skew_gt_lease_cannot_livelock_claim_fence_settle` pins the single-clock-domain fix: repo clock 90s ahead of a simulated relay wall clock (lease 30s) — first claim + first fence succeed at the same repo-clock instant.
**Backoff pure-fn**: `relay.rs::audit_backoff` unit tests — `1→1s, 2→2s, 3→4s, 9→256s, 10→300s, i32::MAX→300s`, monotonic and capped, **no clock, no DB** (plus `clamped_lease` bounds test). Parity with the proven sources is asserted in the same test (E3 `retry_delay`: 1→1s, 2→2s, 99→300s).

### A5 — Claim validation rejects wrong iss/aud/scope/sub
**Test**: `tests/claim_validation.rs::{wrong_issuer_is_rejected_before_any_post, missing_audience_is_rejected_before_any_post, missing_audit_scope_is_rejected_before_any_post, wrong_subject_is_rejected_before_any_post, opaque_non_jwt_token_is_rejected_before_any_post}` + positive `valid_token_is_accepted_and_delivery_proceeds`.
**Assert** (per rejection case): **`stub.posts() == 0`** — the POST counter on the audit endpoint is literally 0, i.e. rejected before any POST; the row requeues (transient drift, never dead) or the token is refused. Valid case: token accepted, expected POST count fires, 202 + valid receipt settles. `unauthorized_refreshes_once_and_retries_within_the_attempt` + `refreshed_token_must_repass_claim_validation_before_retry_post` pin the 401-refresh-once path: the refreshed token must re-pass claim validation before its POST.

### A6 — Regression: snaplink usage-relay delivery tests still green
**Assert**:
1. Existing snaplink tests stay green, run via the standard harness (`cargo test --workspace --lib -- --ignored` with `DATABASE_URL`, per `test-integration.sh`): `snaplink_commercial/http.rs` unit tests (`hard_zero_is_not_treated_as_unlimited`, `unlimited_requires_zero_numeric_limits`, `audit_delivery_requires_a_matching_durable_receipt`), `snaplink_commercial.rs` unit tests (`finite_hard_zero_and_unlimited_zero_are_distinct_and_valid`, `delivery_backoff_is_bounded_and_deterministic`), and the PG-gated `snaplink_commercial_db_tests.rs::message_quota_and_snaplink_outboxes_are_transactional` (usage + audit outboxes enqueued in-tx).
2. **No-touch guard**: `git diff` over `crates/aero-server/src/snaplink_commercial/`, `crates/aero-storage/src/snaplink_commercial.rs`, and `migrations/0235_snaplink_commercial_control_plane.sql` is empty (R8). The only permitted aero-server delta is the connector boot wiring (S5).

### A7 — Boot wiring + relay drill (in-repo subset of the 37/37 gate)
**Test**: connector boot wiring (`crates/aero-server/src/bin/main.rs:251-259` shape) + drill bins + the `test-integration.sh` throwaway-DB slot (S4).
**Assert**: with a stub audit endpoint (202 + valid receipt) and a throwaway DB migrated through the full chain (incl. B5-1 0239 when it lands), N enqueued audit rows reach delivered (status 2 per the 0239 enum) with 1:1 `event_id` parity — `SELECT event_id FROM outbox WHERE status = 2` set-equals `SELECT id FROM audit_events` for the batch; no row stuck in 0/1/3. The 37/37 contract list itself is [PROPOSED] (out-of-repo); when it lands it is pinned as a named list into `test-integration.sh` per the proposal's line 15. Until then the drill above is the in-repo subset.

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| A1 403→dead ≤1 (T-11) | `tests/state_machine.rs::forbidden_dead_on_first_attempt` | `cargo test -p aero-audit-connector`, fake + stub, no DB |
| A2/A3 422/409/receipt → dead ≤1 | `tests/state_machine.rs::permanent_error_dead_after_exactly_two_attempts` (3 arms) | same |
| A4 fencing + pure backoff | `tests/state_machine.rs::{stale_token_cannot_ack_after_reclaim, skew_gt_lease_cannot_livelock_claim_fence_settle}`, `src/relay.rs` backoff/clamp unit tests, `src/pg.rs::concurrent_double_claim_across_two_sessions_is_impossible` (`--ignored`) | unit: no DB; PG test: `DATABASE_URL` + `-- --ignored` |
| A5 claim validation (iss/aud/scope/sub, POST counter == 0) | `tests/claim_validation.rs` (6 rejection + 3 refresh/positive + `client_classifies_statuses`, `payload_guard_rejects_tenant_selection_as_permanent`) | same |
| A6 snaplink usage-relay regression + no-touch | existing snaplink unit/db tests; review gate on `git diff` | `cargo test --workspace --lib -- --ignored` via test-integration.sh |
| A7 boot wiring + relay drill | `main.rs` wiring + drill bins + `test-integration.sh` slot | integration, throwaway DB + stub sink |

## 7. Risks / [PROPOSED] items

- **Exact 422/409 semantics and claim-contract text live in out-of-repo v2 docs** (E6 of the proposal). This spec pins the in-repo normative reading: permanent-class ⇒ requeue once ⇒ dead (≤2 total attempts); 403 ⇒ dead on attempt 1. If the contract text contradicts this reading, only R5's thresholds change — A2/A3's parameterized test is the enforcement point.
- **"37/37" test list is out-of-repo** — A7 pins the in-repo drill now and the full list when the contract text lands.
- **Status enum (0/1/2/3, "status 2" = delivered) is B5-1 0239 DDL's**, not this direction's. A7 asserts against B5-1's constants; the drill cannot run fully until 0239 + the PG repo land.
- **Working-tree port is uncommitted and un-reviewed**: the spec's acceptance set is the review checklist. The direction's proposed claim ("crate does not exist yet") is superseded by S4; the port must be verified against R1–R9, not assumed correct.
- **`is_dead_at` as a named pure fn**: the working-tree port currently inlines the threshold (`attempts < 2` in `deliver_claim`) — R5 requires the `webhook_delivery.rs`-style pure predicate with its own unit test (small delta to the port, explicitly demanded by the direction).
- **JWT claim decode is base64url-only (no signature verification)**: v1 `validate_token` is shape-only; the connector's claim validation decodes the payload and compares iss/aud/scope/sub against configured expectations. Signature verification is the IdP side (B4-2 scope registry); in-repo, fail-closed = no POST on any mismatch (A5).
- **Duplication is deliberate**: A6 forbids touching the snaplink relay, so the connector re-implements the proven SQL shape in its own outbox; the duplication is pinned by A1–A5 mirroring the proven machines' assertions.

## 8. Sequencing

1. **This direction**: verify/complete the working-tree port against R1–R9 — the pure `is_dead_at` fn (R5), any missing A-assertions — with A1–A5 green (`cargo test -p aero-audit-connector`; current state 24/24 + 1 ignored).
2. **B5-1 lands (0239 DDL + repo + status enum + RTMP lifecycle in-tx audit)**: bind the PG impl to the real governance table, run A7's drill in `test-integration.sh` (part 2; part 1 placeholder until the contract list lands).
3. **B5-3 (priority claim)** and **B5-4 (provisioning gate)** proceed in parallel — neither changes the connector's state machine; B5-4's gate consumes this direction's 403→dead fail-closed behavior (T-11).
4. **A6** is a standing gate at every step: snaplink tests green + no-touch diff on the usage relay.
