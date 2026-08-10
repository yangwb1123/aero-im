# Design — aero-audit-connector (B5-2): leased-relay state machine + claim-validated cc delivery

- **Source**: `docs/requirements/2026-08-06-aero-audit-connector.req.md` (R1–R9, A1–A4), campaign `aero-im-b5-outbox-relay`
- **Module**: new crate `crates/aero-audit-connector` (B5-2); **zero aero-ai / ai_usage / v1-snaplink changes**
- **Status**: Design (all cited evidence re-verified 2026-08-06 against master `d5acefe`)

## 0. Evidence verification verdict (the 6 cited items + 4 supplementary)

| Evidence | Claimed | Verified | Notes |
|---|---|---|---|
| E1 `crates/aero-storage/src/ai_usage.rs` | constants L14-15; claim ~475; settle ~497; requeue ~574; backoff ~683 | ✅ exact | `MAX_LEASE_SECONDS=86_400` L14, `MAX_BACKOFF_SECONDS=300` L15; `claim_due` 451, `settle` 497, `mark_failed` 576, `ai_usage_backoff` 683, `clamped_lease` 692. Doc comment 569-570: "deliberately no dead-letter" confirmed |
| E2 `crates/aero-ai/src/usage.rs` | `UsageSink` 56-81; `for_request` 124; `usage_id_for_job` 147 | ✅ exact | reserve 57 / finalize 59 / cancel 66 / default persist 71; `USAGE_NAMESPACE` 86; deterministic v5; `usage_id_for_moderation` 155; `PgUsageSink` at `aero-server/src/ai_usage.rs:96`, wired `bin/boot/services.rs:181` |
| E3 `aero-server/src/snaplink_commercial/http.rs` | `SCOPE_AUDIT` L23; cc grant L233 | ✅ exact (L23, L233) | ⚠️ validator line refs drifted: `validate_delivery_payload` is 387 (claimed 311-329), `validate_audit_receipt` 410 (claimed 330-351), `validate_token` 430 (shape-only — confirmed: empty/len/control-chars/bearer-type only ⇒ **iss/aud/scope/sub validation genuinely new**) |
| E4 `migrations/0235_snaplink_commercial_control_plane.sql` | outbox DDL 161-184, `UNIQUE(destination, idempotency_key)` | ✅ exact | claim-state CHECK (token+lease both-or-neither), due idx `WHERE delivered_at IS NULL`, **no status enum, no dead terminal** (delivered_at only) confirmed |
| E5 `crates/aero-storage/src/ai_job.rs::fail` | L205 dead-terminal precedent | ✅ exact | `status = CASE WHEN attempts >= $3 THEN 'dead' ELSE 'queued' END` |
| E6 `docs/proposals/audit-contract-batch-aero-im.md` | 15-line gate summary; 37/37 out-of-repo | ✅ confirmed | B5-2 semantics (lease > 2×timeout, cap 300s, 422/409/回执错 → dead ≤1 次, 403 → dead T-11) stated; contract text + 37/37 list genuinely out-of-repo |
| E7 config bail | `snaplink_commercial/config.rs:246-251` | ✅ exact | `bail!` unless `delivery_lease > 2*request_timeout + 2s`; second bail (253-255) for task drain |
| E9 db_tests | enumerated | ✅ exact | `backoff_is_bounded_and_exponential` is a **plain `#[test]`** (tests.rs:23, no DB): 1→1s, 2→2s, 3→4s, `i32::MAX`→300s. 8 `#[tokio::test] #[ignore]` PG tests at 35/101/140/181/233/250/275/297 |
| E10 harness | `test-integration.sh`, 0239 absent | ✅ exact | throwaway-DB + `assert_disposable_db_name`; migrations max = 0238, next = 0239 |
| v1 status classification | — | ⚠️ finding | v1 `deliver` (http.rs:133-180) bails on **any** non-202 — it does **not** classify 403/422/409. The permanent-class semantics (403→dead, 422/409→dead ≤1) are therefore genuinely new, not a v1 clone. v1's 401 handling = invalidate + bail (row re-parked); the connector's "refresh once + retry in-attempt" is new and only safe because of the lease>2×timeout invariant |

**Spec-internal discrepancies found (corrected in this design, §6):**
1. **A1-2 backoff assertion**: spec says `backoff(9) == backoff(i32::MAX) == 300s`. Actual `ai_usage_backoff`: `shift = (attempts-1).clamp(0,30); 1<<shift` capped at 300 ⇒ `backoff(9) = 2^8 = 256s`, `backoff(10) = 300s`. **Corrected assertion: offsets 1,2,4,…,256s for attempts 1..9, then 300s from attempts 10; `backoff(10) == backoff(i32::MAX) == 300s`.**
2. **A1-2 "1..9 → then 300s"**: consistent with the correction above (300s appears at attempt 10); the equality clause was the typo.

## 1. Architecture overview

```
audit_events ──(B5-1 enqueue, 0239 CREATE OR REPLACE)──▶ governance outbox (status 0/1/2/3)
                                                                │
    aero-audit-connector (new crate) ◀── OutboxRepo trait ─────┘
        ├─ claim_due (lease + gen_random_uuid() token + SKIP LOCKED)      [clone ai_usage.rs:451]
        ├─ deliver_claim: cc token → claim validation → POST → classify   [v1 http.rs + NEW R6/R5]
        ├─ settle (one-tx fenced ack → status 2)                          [clone ai_usage.rs:497]
        ├─ requeue (fenced re-park, backoff ≤300s)                        [clone ai_usage.rs:576]
        └─ mark_dead (fenced terminal → status 3)                         [NEW; CASE precedent ai_job.rs:205]
                                                                                │
    boot: aero-server bin/boot spawns relay task (CancellationToken)  ───┘
    drill: scripts/test-integration.sh throwaway-DB run (A3)
```

The state machine, fencing contract, backoff, and lease semantics are **cloned from `AiUsageRepo`** (proven + db-tested); the **dead terminal and claim validation are the new surface**. The connector never touches `snaplink_delivery_outbox` (v1) nor any usage-relay code.

## 2. API changes

### 2.1 Workspace
- `Cargo.toml` `[workspace].members` += `"crates/aero-audit-connector"` (after `crates/aero-ai`).
- New crate deps (all already in the lockfile — **zero new third-party crates**): `tokio`, `futures`, `async-trait`, `reqwest` (json), `sqlx` (postgres, runtime-tokio, uuid, time), `serde`/`serde_json`, `thiserror`, `anyhow`, `tracing`, `time`, `uuid`, `base64 = "0.22"` (already pinned by aero-server/aero-live-srt; used for unverified JWT payload decode — `jsonwebtoken` 9.3.1 has no unverified-decode API, only `decode` (needs key) + `decode_header`).
- No `aero-common`/`aero-storage` deps needed (sqlx `PgPool` is used directly); `aero-server` gains `aero-audit-connector.workspace = true` for boot wiring.

### 2.2 `OutboxRepo` trait + claim types (`src/outbox.rs`)
```rust
pub struct Claim {
    pub event_id: uuid::Uuid,          // stable id = audit_events.id (B5-1 parity)
    pub claim_token: uuid::Uuid,       // rotated per claim, gen_random_uuid()
    pub lease_expires_at: time::OffsetDateTime,
    pub attempts: i64,                 // post-increment value (claim.attempts = delivery attempt #)
    pub payload: serde_json::Value,
}

#[async_trait]
pub trait OutboxRepo: Send + Sync {
    async fn claim_due(&self, now: OffsetDateTime, lease: Duration, limit: i64) -> Result<Vec<Claim>, Error>;
    async fn settle(&self, event_id: Uuid, claim_token: Uuid) -> Result<bool, Error>;
    async fn requeue(&self, event_id: Uuid, claim_token: Uuid, attempts: i64,
                     now: OffsetDateTime, error: &str) -> Result<bool, Error>;
    async fn mark_dead(&self, event_id: Uuid, claim_token: Uuid, attempts: i64,
                       error: &str) -> Result<bool, Error>;
}
```
Signatures mirror `AiUsageRepo::{claim_due, settle, mark_failed}` (E1) + the new dead transition. **Fencing contract (R2)**: every mutating op returns `false` (never error) when the fence fails — `claim_token` mismatch, `attempts` mismatch, row no longer claimable, or `lease_expires_at <= clock_timestamp()` — so a late re-park from a superseded attempt cannot overwrite a newer claim, and a stale token can never ack (proven by `stale_claim_cannot_ack_after_reclaim`, E9).

### 2.3 PG impl (`src/pg.rs`) — binds B5-1's 0239 governance outbox
SQL cloned from `ai_usage` (E1) against the 0239 table (owned by B5-1; connector owns only these statements — R1 ownership decision, fixed at integration):
- `claim_due`: `WITH claimable AS (SELECT ... WHERE status IN (0,1) AND available_at <= $1 AND (lease_expires_at IS NULL OR lease_expires_at <= $1) ORDER BY available_at, created_at, event_id FOR UPDATE SKIP LOCKED LIMIT $2) UPDATE ... SET claim_token = gen_random_uuid(), lease_expires_at = $3, attempts = attempts + 1 ... RETURNING ...` — `limit.clamp(1, 500)`, lease clamped `[1, 86_400]`.
- `settle`: one tx — fenced re-read `FOR UPDATE` → (no ledger for audit; the sink's durable receipt is the record) → ack `SET status = 2, delivered_at = clock_timestamp(), claim_token = NULL, lease_expires_at = NULL, last_error = NULL` with `rows_affected == 1` else rollback + `Ok(false)`.
- `requeue`: fenced `UPDATE ... SET available_at = $4 (now + audit_backoff(attempts)), claim_token = NULL, lease_expires_at = NULL, last_error = truncate(error, 2048)` on `event_id AND claim_token AND attempts AND status IN (0,1) AND lease_expires_at > clock_timestamp()`.
- `mark_dead`: same fence, `SET status = 3, delivered_at = NULL, claim_token = NULL, lease_expires_at = NULL, last_error = ...`.
- Status constants are **B5-1's** (0=enqueued, 1=claimed, 2=delivered, 3=dead per proposal); the connector maps them via named constants and the A3 drill pins `2 = delivered`.

### 2.4 In-memory fake (`src/fake.rs`) — public test double
Rows `{event_id, status, available_at, attempts, claim_token, lease_expires_at, last_error, payload}` implementing the trait with the **same transition semantics** as the PG impl (deterministic `now` injection). Used by A1; also usable by aero-server tests.

### 2.5 HTTP client (`src/client.rs`) — cc token + claim validation + receipt
```rust
pub enum PermanentKind { Unprocessable, Conflict, ReceiptMismatch }      // 422, 409, receipt error
pub enum DeliveryError {
    Transient(anyhow::Error),        // transport, timeout, 5xx, 401-after-refresh, claim-validation-fail
    Permanent(PermanentKind),        // → requeue once → dead (R5)
    Forbidden,                       // 403 → dead immediately (T-11)
}
impl AuditClient {
    pub async fn token(&self) -> Result<String, Error>;                       // cc + cache + 401 invalidate (v1 clone)
    pub async fn validate_token_claims(&self, token: &str) -> Result<(), ClaimRejection>; // NEW, pure, unit-testable
    pub async fn deliver(&self, claim: &Claim) -> Result<(), DeliveryError>;  // validate → POST → classify
}
```
- Token request (v1 clone, http.rs:222-254): `basic_auth(client_id, client_secret)` + form `grant_type=client_credentials`, `scope=SCOPE_AUDIT` (`"audit:event:write"`), `resource=audit_resource`; cache keyed `(scope, resource)` with ttl-based `refresh_at` (`usable = ttl>60 ? ttl-30 : max(ttl/2,1)`) and 401 invalidation.
- **Claim validation (R6, NEW vs v1's shape-only `validate_token`)** — before **every** delivery POST, decode the JWT payload (base64url, unverified signature — same trust posture as v1; JWKS verification is [PROPOSED], §7) and require: `iss == configured issuer`, `aud` contains configured audience (string or array), `scope` contains `audit:event:write` (space-delimited string or array), `sub == configured identity`. Mismatch ⇒ `ClaimRejection` ⇒ fail-closed: **no POST**, token discarded, error classified `Transient` (row requeued with backoff — the fault is IdP config drift, not the row; loud error log; provisioning gate is B5-4).
- Delivery (v1 clone + R5/R6): `Bearer` + `Idempotency-Key: {event_id}` + payload; expected `202 ACCEPTED`. `401` ⇒ invalidate cache, refresh once, retry once **within the same attempt** (safe: lease > 2×timeout + 2s guarantees two request timeouts fit in one lease — R3); still failing ⇒ `Transient`. Status classification: 403 ⇒ `Forbidden`; 422 ⇒ `Permanent(Unprocessable)`; 409 ⇒ `Permanent(Conflict)`; other 4xx (400/404/405/410…) ⇒ `Transient` + warn (see §7 open decision); 5xx/transport/timeout ⇒ `Transient`.
- Payload guard (v1 clone, http.rs:387): no `tenant_id` selection; `source_system` == trusted binding. Receipt guard (v1 clone, http.rs:410): `event_id` == claim, `tenant_id` == claim, `accepted_at` present, `conflict == false`, `status ∈ {ledgered, indexed, archived}`; body capped (`MAX_AUDIT_RECEIPT_BODY`); any failure ⇒ `Permanent(ReceiptMismatch)`.

### 2.6 Relay loop (`src/relay.rs`)
```rust
pub struct AuditRelay { repo: Arc<dyn OutboxRepo>, client: AuditClient, config: RelayConfig }
impl AuditRelay {
    pub fn spawn(self, cancel: CancellationToken) -> JoinHandle<()>;   // poll loop, Skip on miss, graceful drain
    pub async fn dispatch_batch(&self) -> Result<usize, Error>;        // claim ≤ batch → for_each_concurrent → classify
    async fn deliver_claim(&self, claim: Claim);                       // exactly one of settle/requeue/mark_dead
}
pub fn audit_backoff(attempts: i64) -> Duration;                       // 2^(attempts-1), cap 300 (parity fn, exported)
pub const MAX_LEASE_SECONDS: i64 = 86_400;  pub const MAX_BACKOFF_SECONDS: i64 = 300;
pub const MAX_CLAIM: i64 = 500;
```
Per-claim outcome → exactly one transition:
- `deliver == Ok` ⇒ `settle`; fence-lost ⇒ warn ("lease expiry will reclaim"), **at-least-once covered by `Idempotency-Key: event_id`** at the sink.
- `deliver == Transient` ⇒ `requeue` (backoff); fence-lost ⇒ warn.
- `deliver == Permanent(kind)` ⇒ `requeue` if `claim.attempts < 2` else `mark_dead` (dead after ≤1 retry; `last_error` = kind).
- `deliver == Forbidden` ⇒ `mark_dead` unconditionally (attempts ≥ 1; T-11 fail-closed).

### 2.7 Config (`src/config.rs`) — fail-loud
Presence-gated activation: **feature is ON iff `AERO_AUDIT_TOKEN_ENDPOINT` is set**; any `AERO_AUDIT_*` var set while incomplete/malformed ⇒ boot `bail!` (fail-loud, v1 `config.rs` precedent). Env surface (plain single-underscore, per AGENTS.md §4.3 exceptions — NOT `AERO__`):

| Env | Default | Range/valid |
|---|---|---|
| `AERO_AUDIT_TOKEN_ENDPOINT` | — (presence gates) | URL |
| `AERO_AUDIT_EVENTS_URL` | — | URL |
| `AERO_AUDIT_RESOURCE` | — | non-empty |
| `AERO_AUDIT_CLIENT_ID` / `AERO_AUDIT_CLIENT_SECRET` | — | non-empty |
| `AERO_AUDIT_EXPECTED_ISS` / `AERO_AUDIT_EXPECTED_AUD` / `AERO_AUDIT_EXPECTED_SUB` | — | non-empty |
| `AERO_AUDIT_EXPECTED_SCOPE` | `audit:event:write` | non-empty |
| `AERO_AUDIT_REQUEST_TIMEOUT_SECS` | 10 | 1..120 |
| `AERO_AUDIT_DELIVERY_LEASE_SECS` | 30 | 5..300; **bail unless > 2×timeout + 2s** (R3) |
| `AERO_AUDIT_TASK_DRAIN_SECS` | 30 | **bail unless > 2×timeout + shutdown_drain + 2s** (second v1 bail, config.rs:253-255) |
| `AERO_AUDIT_BATCH_SIZE` | 100 | 1..500 (MAX_CLAIM) |
| `AERO_AUDIT_CONCURRENCY` | 4 | 1..32 |
| `AERO_AUDIT_POLL_INTERVAL_SECS` | 5 | 1..300 |

### 2.8 Boot wiring (aero-server) + drill bin
- `crates/aero-server/src/bin/boot/`: spawn `AuditRelay::spawn(cancel)` when config present (alongside existing loops; shared `CancellationToken` for graceful drain — AGENTS.md §4.3). The claim loop logs and retries on SQL error (does not panic), so booting before 0239 exists degrades to logged errors, not crash.
- `crates/aero-audit-connector/src/bin/aero-audit-relay-drill.rs`: standalone drill binary — connect to `DATABASE_URL` DB, seed N rows via B5-1's enqueue function, run the relay for a bounded budget against a stub sink, assert `COUNT(status=2) == N` + event_id set-parity, exit 0/1. Used by A3; doubles as a staging harness (run the relay standalone against a real sink without booting aero-server).
- `scripts/test-integration.sh`: add `AUDIT_CONNECTOR_INTEGRATION_DB="aero_audit_connector_$$"` + `assert_disposable_db_name` guard + drill section + pinned contract-list placeholder (A3 part 1).

## 3. Compatibility constraints

1. **Zero code changes to**: `crates/aero-storage/src/ai_usage.rs`, `crates/aero-ai/**` (usage.rs is a read-only pattern source), `crates/aero-server/src/ai_usage.rs` (`PgUsageSink`), `crates/aero-server/src/snaplink_commercial/**` (v1 relay + usage destination path). Enforced by A4 no-touch `git diff` guard; boot wiring is the only permitted aero-server delta (new dep + spawn call).
2. **No writes to v1 tables**: the connector claims only the B5-1 governance outbox; `snaplink_delivery_outbox` and its 0236-triggered enqueue stay untouched (both tables coexist during cutover; trigger redirect is B5-1's).
3. **No new third-party deps**: `base64 0.22` already in lockfile (aero-server/aero-live-srt); everything else workspace-pinned. MSRV 1.80 / edition 2021 / `unsafe_code = forbid` / clippy `all`+`pedantic` warn — no new warnings (`cargo clippy --workspace --all-targets` gate).
4. **Layering**: connector depends on sqlx directly, not on aero-storage's `AiUsageRepo` (R8: deliberately does not import it). Dependency direction: `aero-server → aero-audit-connector`; `aero-audit-connector → {sqlx, reqwest, ...}` only. B5-1 (aero-storage) must **not** depend on the connector — hence the trait + PG impl both live in the connector crate; B5-1 owns only DDL/enqueue/reconcile.
5. **At-least-once posture preserved**: crash between POST and settle ⇒ row re-claimed with new token ⇒ re-POST with same `Idempotency-Key: event_id` ⇒ sink dedups. Transients requeue forever (v1 posture, E8); only permanent classes and 403 reach the dead terminal.
6. **Config coexistence**: `AERO_AUDIT_*` is disjoint from v1's `AERO_SNAPLINK_*`; both relays can run simultaneously during cutover.

## 4. Failure modes (state machine table)

| # | Failure | Class | Transition | Terminal? |
|---|---|---|---|---|
| F1 | 202 + valid receipt | success | `settle` → status 2 | yes |
| F2 | 202 + receipt mismatch (event_id/tenant_id mismatch, missing `accepted_at`, `conflict=true`, status ∉ {ledgered,indexed,archived}, body over cap) | Permanent(ReceiptMismatch) | attempts ≥ 2 ⇒ `mark_dead`; else `requeue` | after ≤1 retry |
| F3 | HTTP 422 | Permanent(Unprocessable) | same as F2 | after ≤1 retry |
| F4 | HTTP 409 | Permanent(Conflict) | same as F2 | after ≤1 retry |
| F5 | HTTP 403 | Forbidden | `mark_dead` on attempt 1 (T-11 fail-closed, no requeue) | yes, immediate |
| F6 | HTTP 401 | transient | invalidate cached token → refresh once → retry once in-attempt (lease > 2×timeout makes this safe); still failing ⇒ `requeue` | never |
| F7 | Claim validation fail (iss/aud/scope/sub mismatch, non-JWT token) | transient | **no POST** (fail-closed); token discarded; `requeue` + loud error log | never (config drift, fixed by B5-4) |
| F8 | 5xx / transport / timeout / connection | transient | `requeue` with `audit_backoff(attempts)` (1,2,4,…,300s) | never |
| F9 | Other 4xx (400/404/405/410…) | transient (default, §7) | `requeue` + warn | never |
| F10 | Local payload-guard violation (tenant_id selected, source_system mismatch) | Permanent | attempts ≥ 2 ⇒ dead; else requeue | after ≤1 retry |
| F11 | Lease expiry while in-flight (crash/超时 worker) | — | row reclaimable; new `gen_random_uuid()` token; old token can never settle/requeue/dead (fence) | — |
| F12 | Fence lost on settle/requeue/mark_dead (superseded attempt) | — | return `false` ⇒ warn "lease expiry will reclaim" | — |
| F13 | Claim SQL error (e.g., 0239 table missing at boot) | — | loop logs error, sleeps poll interval, retries; no panic, no state change | — |

Dead rows (status 3) are excluded from `claim_due` forever; `last_error` records the terminal class (F2–F5, F10). Retention/cleanup of dead rows = ops/B5-1 concern (out of scope).

## 5. Migration steps

**Step 0 — calibration**: `git reset --hard master` (multi-agent rule); confirm `ls migrations/*.sql` max = 0238 (next = 0239, owned by B5-1 — **no migration lands in this direction**).

**Step 1 — crate skeleton (no PG, no B5-1 dependency)**:
1. Root `Cargo.toml` members += `crates/aero-audit-connector`; create `crates/aero-audit-connector/{Cargo.toml, src/{lib.rs, outbox.rs, fake.rs, client.rs, relay.rs, config.rs, error.rs}}`.
2. Implement trait + fake + `audit_backoff` + config (both bails) + client (cc, claim validation, classification) + relay loop.
3. Gates: `cargo check --workspace` clean · `cargo clippy --workspace --all-targets` no new warnings · `scripts/{truth-check,file-size-check}.sh` 0 violations (Rust 800 WARN / 1200 HARD per file).

**Step 2 — A1 + A2 green** (`cargo test -p aero-audit-connector`, no DB): fake-outbox state machine + claim-validation stub tests (§6).

**Step 3 — boot wiring**: `aero-server` dep += connector; `bin/boot/` spawns the relay on config presence with the shared `CancellationToken`. Gate: full `cargo test --workspace --lib` green; A4 no-touch diff empty (except the wiring delta).

**Step 4 — B5-1 integration (after 0239 + `audit_governance.rs` land)**: implement `PgOutboxRepo` against the governance outbox (status constants from B5-1; `event_id` = `audit_events.id`); add the drill bin; add A3 section to `scripts/test-integration.sh` (throwaway DB `aero_audit_connector_$$`, migrate, seed N, run drill, assert, drop DB) + pin the contract-test list placeholder (part 1).

**Step 5 — cutover (campaign-level, NOT this direction)**: retire v1 embedded audit relay + 0236-trigger redirect (B5-1); both tables coexist during transition; connector never claims v1 rows.

Sequencing note: A4 is a standing gate for every step (`--ignored` PG suite green + no-touch diff).

## 6. Testable acceptance mapping

### A1 — fake-outbox unit tests (`crates/aero-audit-connector/tests/state_machine.rs` or inline `#[cfg(test)]`, tokio, no DB)
| # | Test | Setup → Act | Assert |
|---|---|---|---|
| A1-1 | `stale_token_cannot_ack_after_reclaim` | row, lease=1s; claim at t0 → token A; claim at t0+2s → token B | `token_a != token_b`; `settle(id, A)` == `false`; `settle(id, B)` == `true`; row not claimable afterwards |
| A1-2 | `backoff_is_bounded_and_exponential` | — | `audit_backoff(1..9)` == 1,2,4,8,16,32,64,128,256s; `audit_backoff(10) == audit_backoff(i32::MAX) == 300s` ⚠️ **corrected from spec's `backoff(9)==300s`**; requeue re-parks `available_at = now + backoff(attempts)` |
| A1-3 | `permanent_error_dead_after_exactly_two_attempts` | parameterized over {422, 409, receipt-mismatch} | attempt 1 (attempts=1) ⇒ requeued, not dead, `available_at` advanced by backoff(1)=1s; attempt 2 (attempts=2) ⇒ `mark_dead` called, row excluded from `claim_due`, `last_error` set |
| A1-4 | `forbidden_dead_on_first_attempt` | 403 on attempt 1 | dead directly; exactly 1 attempt recorded; no requeue; excluded from `claim_due` |
| A1-5 | `happy_path_settles_and_removes_from_claimable` | 202 + valid receipt | `settle` true; `claim_due` returns empty |

### A2 — claim-validation tests (stub token endpoint issues JWT with per-case claims; stub audit endpoint counts POSTs)
| Case | Token claims | Assert |
|---|---|---|
| a | wrong `iss` | **POST counter == 0** |
| b | `aud` missing configured audience | **POST counter == 0** |
| c | `scope` missing `audit:event:write` | **POST counter == 0** |
| d | wrong `sub` | **POST counter == 0** |
| e | all correct | POST counter ≥ 1; delivery proceeds |

"Rejected before any POST" is literally measured. Stub servers are hand-rolled `tokio::net::TcpListener` HTTP responders (~60 lines, no new dev-deps).

### A3 — integration drill (`scripts/test-integration.sh` + drill bin)
- Part 1 (pin): contract "37/37" list is [PROPOSED] (E6) — placeholder named list in the script; filled when the contract text lands.
- Part 2 (drill): throwaway DB `aero_audit_connector_$$` (guard via `assert_disposable_db_name`) → full migration chain (incl. 0239) → seed **N** rows (N = pinned count; 37 when the list lands, else in-repo subset count) → run `aero-audit-relay-drill` against stub sink (202 + valid receipt) →
  - `SELECT COUNT(*) FROM governance_outbox WHERE status = 2` == **N**
  - `event_id` set-parity: `SELECT event_id WHERE status = 2` set-equals `SELECT id FROM audit_events` for the batch (no duplicates, no orphans)
  - any row stuck in 0/1/3 or parity mismatch ⇒ script exit non-zero.

### A4 — regression + no-touch guard
1. Green via `scripts/test-integration.sh` (`DATABASE_URL` + `--ignored`): the 8 ai_usage db_tests (35/101/140/181/233/250/275/297) + `backoff_is_bounded_and_exponential` (23) + v1 snaplink unit tests (`hard_zero_is_not_treated_as_unlimited`, `unlimited_requires_zero_numeric_limits`, `audit_delivery_requires_a_matching_durable_receipt`).
2. `git diff --stat -- crates/aero-storage/src/ai_usage.rs crates/aero-ai/ crates/aero-server/src/ai_usage.rs crates/aero-server/src/snaplink_commercial/` == empty (only the wiring delta in `bin/boot/` + `aero-server/Cargo.toml` permitted).

## 7. Open decisions ([PROPOSED], no in-repo authority)

1. **Unspecified 4xx (400/404/405/410)**: default = `Transient` + warn (never dead). If the out-of-repo contract promotes any of these to permanent, only the classifier changes (A1-3's parameterized table is the enforcement point).
2. **JWT signature verification**: this design validates claims on an unverified base64url decode (same trust posture as v1's bearer shape check — the IdP is trusted via client_credentials). JWKS verification would need a new jose dep or hand-rolled RSA — out of scope; flag for B5-4/provisioning review.
3. **Status enum values**: 0/1/2/3 and "2 = delivered" are B5-1's DDL; the connector maps via named constants and A3 pins the mapping. If B5-1 chooses a different layout, only `src/pg.rs` constants change.
4. **Dead-row retention**: no cleanup in this direction (ops/B5-1).
5. **`backoff(9)==300s` spec typo**: corrected to `backoff(10)==300s` (A1-2); if the contract text contradicts, the parameterized assertion is the enforcement point (per spec §7 risk note).
