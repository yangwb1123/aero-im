# Design — Landing the auth/admin in-tx audit producers (B5-1 auth slice)

> Date: 2026-08-15 · Status: **Proposed** · Module: `crates/aero-ai` direction, landing in `aero-storage` / `aero-auth` / `aero-server`
> Governing designs (both un-landed, verbatim normative): `2026-08-08-aero-auth-b5-1-in-tx-audit-outbox.design.md` (auth design) and `2026-08-08-aero-audit-connector-b5-1-producer-side.design.md` (connector design); requirements doc `2026-08-08-aero-auth-b5-1-in-tx-audit-outbox.req.md`.
> This document is the **landing design**: it turns the two designed-only slices into concrete API/failure/migration/acceptance steps, with every claim re-verified against the repo on this run.

---

## 1. Evidence verification summary (all claims re-checked this run)

| E# | Claim | Verdict | Anchor |
|---|---|---|---|
| E1 | `governance_lane_for` maps only `message.moderated` → admin; room lanes 0245 | ✅ (line drift only) | `crates/aero-ai/src/governance.rs`: `governance_lane_for` :82; admin :84-86; room :98-107; consts :31/:33; `is_admin_class` :121; `admin_class_rows_never_aggregated` is the **test fn at :238** (not :200-211 — symbol exists, line drifted) |
| E2 | 0239 trigger allowlist = `message.moderated` only, Gate 1 fail-open / Gate 2 RAISE fail-closed, `ON CONFLICT (event_id) DO NOTHING` | ✅ verbatim | `migrations/0239_audit_governance_outbox.sql` |
| E3 | 0245 header records 0243/0244 as D8-finalized designed-only | ✅ verbatim | `migrations/0245_room_audit_governance.sql` :4-11 |
| E4 | 244 migrations; no 0243/0244 | ✅ | `ls migrations/*.sql \| wc -l` = 244; last = 0245, 0246 |
| E5 | PAT in-tx `auth.pat.create`/`auth.pat.revoke` | ✅ | `pat.rs` create :104/:135, revoke :190 |
| E6 | TOTP in-tx `auth.totp.enabled`/`auth.totp.disabled` | ✅ | `totp.rs` activate :122/:141, disable :165-184 |
| E7 | `auth.register` in-tx | ✅ | `registration.rs` create :39/:121; `NewRegistration` :15; 3 ctors :177/:240/:361 + `register_enrolled` :222-245 |
| E8 | session-revoke producers (F-1 amended) | ✅ amended | `auth_session.rs:499` is `revoke_and_blacklist` CTE SQL (not an audit producer); `session.rs:212/:220-236` pool-level logout; `sessions.rs:108/:114-135` pool-level inventory (already §2.5-shaped: target=session_id, detail `{"session_id":…}`); `auth_session/admin_revoke.rs:112` **in-tx** |
| E9 | `admin_sessions.rs` exists, route-side zero audit | ✅ amended | `admin_sessions.rs` force-revoke route :44, `authorize_global_revoke` :67; storage mutation is `admin_revoke.rs:112` (in-tx, outbox-less) |
| E10 | §2.7 token table vs landed emitters conflict | ✅ | §2.7 pins `auth.pat.issue`/`auth.totp.enroll`; landed emitters write `auth.pat.create`/`auth.totp.enabled`/`auth.totp.disabled`; `audit_tokens.rs` `AUTH_PAT_CREATE` :33, `AUTH_TOTP_ENABLED` :38, `AUTH_TOTP_DISABLED` :40, pin test :67+, `SERVER_OWNED` :31-35 keeps `session.revoked` out of vocabulary |
| E11 | Login: 2FA gate handler-side; no `auth.login` success row | ✅ | `handlers/auth.rs` 2FA gate :150-199, `finalize_login(true)` :197, `record_session` :201, `record_login_event` :208 (`auth.login.new_ip` :59, `auth.login.recovery_code` :221, pool-level, `DEFAULT_WORKSPACE_ID`) |
| E12 | `AuthService::refresh` (:334 → :399) | ✅ symbol exact, line drifted | `aero-auth/src/service.rs` `refresh` :399 |
| E13 | `login_failures` (0140) no single-column `created_at` index; sweep | ✅ | 0140: `(account, created_at DESC)` + `(ip, created_at DESC)` only; `LoginFailureRepo::record` :63; sweep `bin/boot/retention.rs` :52/:416, env `AERO__SERVER__LOGIN_FAILURE_RETENTION_DAYS` (env-only — **no config.toml key exists**, so the new L1 env follows the same env-only pattern) |
| E14 | 0242 L1 aggregate + `aggregated,true` exemption key | ✅ | `0242`: `aero_enqueue_l1_aggregate_audit()` :64, top-level `'aggregated', true`; `L1_WINDOW_SECONDS=60` `model/audit.rs:185`; db_test `l1_window_aggregates_5_rows_to_1_outbox` |
| E15 | parity db_test pattern + H3 reservation | ✅ | `db_tests/parity.rs`, `lanes.rs`, `db_tests.rs` helpers :23/:96/:145/:207/:307, `producer/{send,room,carveout}.rs`; `audit_governance/mod.rs` :22-23 reserves H3 |
| E16 | harness: `run_migrated_integration` (4th param, default `-p aero-storage`), B5-1 segment :317-336, b5-pin **40 slots (19 executed + 21 PROPOSED)** guard `-ne 40` :91, pin-guard "40/40 (19 executed…)" :64, t11 drill seeds `auth.login.failed` class message/10 | ✅ (F-2 confirmed) | `scripts/test-integration.sh`, `scripts/b5-pin.sh`, `scripts/test-b5-pin-guard.sh`, `aero-audit-t11-drill.rs` :17 |
| E17 | Connector status machine 0/1/2/3, `priority DESC` claim | ✅ | `aero-audit-connector/src/pg.rs` :27-30, claim :98, ORDER BY priority DESC :129 |
| E18 | `audit_events.workspace_id NOT NULL`; nil workspace seeded | ✅ | `migrations/0007_audit.sql` :10; `0006_workspaces.sql` nil row; `WorkspaceId::from_uuid(uuid::Uuid::nil())` at `session.rs:221` |
| E19 | Both metrics absent (designed-only, D10) | ✅ | `rg` zero hits in `crates/`; storage has zero metrics today |

**Findings re-confirmed**: F-1 (three-producer claim amended — `auth_session.rs:499` is not a producer; third producer is `admin_revoke.rs:112`, in-tx), F-2 (b5-pin is 40/40 = 19 executed + 21 PROPOSED, not the connector design's 37), F-3 (token conflict, resolved by DP-1 → §2.7 wins).

**New finding (this run, F-4)**: `scripts/test-integration.sh` :613-621 carries a **static migration-count arbiter** `MIGRATION_COUNT -ne 244 → FAIL`. The 0245 header's "arbiter flips in the same commit" pattern applies here: landing 0243+0244 requires flipping the arbiter to **246 in the same commit**. The requirements doc's AC-1 omits this; it is load-bearing (the arbiter would fail the B5 segment otherwise).

**New finding (this run, F-5)**: the working tree carries **in-flight B5-3 edits** (`crates/aero-audit-connector/{fake,outbox,pg,relay}.rs`, `aero-audit-priority-drill.rs`, `scripts/test-integration.sh`, `scripts/truth-check-lib.sh`, two b5-3 design docs — modifications, not deletions). Sequencing §8's "hard prerequisite" (prior batch committed, no in-flight work) is **not currently satisfied**: the prior batch must be committed (or stashed) before this slice starts. This slice touches none of those files except `scripts/test-integration.sh` (adding the `auth_outbox_parity` entry) — the B5-3 edits to that file must land first to avoid clobbering.

---

## 2. API changes

### 2.1 Migrations (2 new files — D8-finalized numbering)

**`migrations/0243_login_failures_created_at_idx.sql`** — exactly one statement:
```sql
-- Migration 0243: login_failures.created_at single-column index (auth B5-1 slice).
-- RENUMBERED from the original 0242 plan (connector-root design §7 D8): 0242 is
-- taken by the aero-ai L1 trigger migration; this slice unconditionally takes
-- 0243 + 0244. Serves the L1 bucket scan (aggregate_login_failure_buckets) and
-- the retention sweep. Idempotent, additive.
CREATE INDEX IF NOT EXISTS login_failures_created_at_idx
    ON login_failures (created_at);
```

**`migrations/0244_audit_governance_failed_pairs.sql`** — DLQ table, DDL verbatim from connector design §5.3 (D8-finalized; **no triggers, no FKs** — anti-recursion: writes happen outside the 0236/0239 trigger scope; anti-blocking: a bad row can never block replay):
```sql
-- Migration 0244: DLQ for fail-open audit pairs (auth B5-1 slice, D9).
-- No triggers, no FKs. Connector never claims this table.
-- IMMEDIATE-constraint pin (D4): 0239/0236 CHECK/RAISE constraints are all
-- IMMEDIATE — a future DEFERRABLE change would defer errors to COMMIT and
-- break the SAVEPOINT fail-open branch (whole-tx fail-closed, violates R7).
CREATE TABLE IF NOT EXISTS audit_governance_failed_pairs (
    id              BIGSERIAL   PRIMARY KEY,
    workspace_id    UUID        NOT NULL,
    actor_id        UUID,
    action          TEXT        NOT NULL,
    target          TEXT,
    detail          JSONB       NOT NULL,
    outbound_action TEXT        NOT NULL,
    error_sqlstate  TEXT        NOT NULL,
    error_message   TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    replayed_at     TIMESTAMPTZ
);
```

### 2.2 aero-storage — `audit_governance/` grows the H3 repo (new files `outbox.rs`, `failed_pairs.rs`, `tokens.rs`, all ≤800 lines, `mod.rs` `pub mod` + `pub use`)

**Token table (D1 — storage is the single Rust source; `aero-auth`'s `audit_tokens.rs` mirrors textually with its pin test as the drift guard; AC-2 asserts field-by-field):**

```rust
// tokens.rs — §2.7 allowlist (verbatim; DP-1 renames applied)
pub const AUTH_REGISTER: &str = "auth.register";            // existing
pub const AUTH_LOGIN: &str = "auth.login";                  // NEW
pub const AUTH_REFRESH: &str = "auth.refresh";              // NEW
pub const AUTH_PAT_ISSUE: &str = "auth.pat.issue";          // RENAMED from auth.pat.create (DP-1)
pub const AUTH_PAT_REVOKE: &str = "auth.pat.revoke";        // existing
pub const AUTH_TOTP_ENROLL: &str = "auth.totp.enroll";      // REPLACES auth.totp.enabled/.disabled (DP-1)
pub const SESSION_REVOKED: &str = "session.revoked";        // existing (D6 continuity)
pub const SESSION_REVOKED_ADMIN: &str = "session.revoked.admin"; // NEW
// outbound
pub const OUTBOUND_AUTH_REGISTER: &str = "admin.auth.register";
pub const OUTBOUND_AUTH_LOGIN: &str = "admin.auth.login";
pub const OUTBOUND_AUTH_REFRESH: &str = "admin.auth.refresh";
pub const OUTBOUND_AUTH_PAT_ISSUE: &str = "admin.auth.pat.issue";
pub const OUTBOUND_AUTH_PAT_REVOKE: &str = "admin.auth.pat.revoke";
pub const OUTBOUND_AUTH_TOTP_ENROLL: &str = "admin.auth.totp.enroll";
pub const OUTBOUND_AUTH_SESSION_REVOKE: &str = "admin.auth.session.revoke";
pub const OUTBOUND_AUTH_LOGIN_FAILURE: &str = "admin.auth.login.failure"; // L1
pub const AUTH_SOURCE_SYSTEM: &str = "aero-auth";           // envelope source_system for auth explicit writes
pub const L1_AUTH_LOGIN_FAILURE_ACTION: &str = "auth.login.failure";
```

**`AuditGovernanceOutboxRepo`** (signatures verbatim from connector design §2.1-§2.2):

```rust
pub struct AuditGovernanceOutboxRepo { pool: PgPool }
impl AuditGovernanceOutboxRepo {
    pub fn new(pool: PgPool) -> Self;

    /// 16-key 0239-shaped envelope, Rust mirror of the trigger's jsonb_build_object:
    /// event_id, source_system, event_type, schema_id, schema_version, occurred_at,
    /// actor{id,type}, targets[], aggregate_type, aggregate_id, action, outcome,
    /// payload, data_classification, retention_class, idempotency_key.
    /// source_system parameterized: auth explicit writes = "aero-auth"; moderation
    /// trigger = binding.source_system. outcome ∈ {"success","failure"}.
    /// L1 rows add top-level "aggregated": true (0242-shared parity-exemption key —
    /// NEVER inside detail).
    pub fn governance_envelope(
        event_id: AuditId, workspace: WorkspaceId, actor: Option<ParticipantId>,
        target: Option<&str>, detail: serde_json::Value, outbound_action: &str,
        outcome: &str, occurred_at: time::OffsetDateTime, source_system: &str,
    ) -> serde_json::Value;

    /// Explicit-event_id outbox insert (replay path); idempotent via 0239
    /// ON CONFLICT (event_id) DO NOTHING. Mirrors AuditRepo::append_in_tx shape.
    pub async fn append_in_tx(tx: &mut Transaction<'_, Postgres>,
        event_id: AuditId, class: &str, priority: i16,
        payload: serde_json::Value) -> Result<(), sqlx::Error>;

    /// R1+R7 single implementation point. class 'admin', priority 10 built in.
    pub async fn append_pair_in_tx_fail_open(tx: &mut Transaction<'_, Postgres>,
        workspace: WorkspaceId, actor: Option<ParticipantId>, action: &str,
        target: Option<&str>, detail: serde_json::Value,
        outbound_action: &str) -> Result<Option<AuditId>, sqlx::Error>;

    /// login/refresh: own tx (begin → pair → commit), swallow-all (D12): Option<AuditId>, no Err.
    pub async fn record_pair_standalone(&self, workspace: WorkspaceId,
        actor: Option<ParticipantId>, action: &str, target: Option<&str>,
        detail: serde_json::Value, outbound_action: &str) -> Option<AuditId>;

    /// L1 auth.login.failure aggregation (D2 outcome A): closed buckets only,
    /// 1 outbox row/bucket, deterministic v5 event_id, class 'message', priority 10.
    pub async fn aggregate_login_failure_buckets(&self, window_secs: i64)
        -> Result<usize, sqlx::Error>;
}

/// Shared classifier (D4/D9): Database-class → fail-open; everything else →
/// propagate (in-tx) / warn+None (standalone). G1 unit test pins the propagation branch.
pub fn is_fail_open_error(err: &sqlx::Error) -> bool;
```

**`append_pair_in_tx_fail_open` — concrete flow** (the only SAVEPOINT in the slice):
```
1. contract pre-check (Rust): detail must be a JSON object.
   Non-object → warn!(sqlstate="contract") + counter{category="contract"} += 1 → Ok(None), no DB touch (F2).
2. SAVEPOINT aero_audit_pair;
3. audit row:  AuditRepo::append_in_tx(tx, workspace, actor, action, target, detail)   // reuse — one audit INSERT impl;
                // returns fresh AuditId + server-stamped created_at; fires 0239/0242/0245 (all pass through for auth tokens)
                // and 0236 v1 (snaplink_delivery_outbox — v1 coexistence, or RAISE P0001, or gate pass-through).
4. outbox row: INSERT INTO audit_governance_outbox (event_id, status, class, priority, payload)
               VALUES (audit_id, 0, 'admin', 10, governance_envelope(..., "aero-auth", outcome "success"));
5. RELEASE SAVEPOINT aero_audit_pair; return Ok(Some(audit_id)).
-- error branches (every non-success branch leaves zero written rows — at-least-once discipline):
Ea. sqlx::Error::Database(e) [is_fail_open_error]:
     ROLLBACK TO SAVEPOINT aero_audit_pair;
     SAVEPOINT aero_audit_dlq;  INSERT INTO audit_governance_failed_pairs
       (workspace_id, actor_id, action, target, detail, outbound_action, error_sqlstate, error_message)
       VALUES (...);  RELEASE SAVEPOINT aero_audit_dlq;
       -- nested-savepoint guard: if the DLQ insert itself fails (e.g. 42P01 — 0244 missing),
       -- ROLLBACK TO aero_audit_dlq + ERROR log (no detail) → still Ok(None): domain commits, pair lost-but-logged.
     classify by PgDatabaseError::code(): P0001 → category "binding" (ERROR);
       23514/23503/23505 → "contract" (ERROR); 40P01/40001/55P03 → "database" (warn);
       other → "database" (ERROR).
     counter{category} += 1; log(workspace_id, action, path="in_tx", error_sqlstate, dlq_id) — detail NEVER logged.
     return Ok(None).   // fail-open: domain row commits
Eb. other sqlx::Error (IO/Protocol/PoolTimedOut/connection-level):
     ROLLBACK TO SAVEPOINT aero_audit_pair; return Err(e).   // caller's tx unusable; domain op fails as today.
```

**`record_pair_standalone` flow**: `pool.begin()` → `append_pair_in_tx_fail_open(&mut tx, …)` → on `Ok(Some(id))` commit → `Some(id)`; on `Ok(None)` rollback → `None` (DLQ row already written in-tx and commits with it); on `Err(e)` (connection-level) rollback + warn → `None` (pair never written; best-effort `FailedPairRepo::enqueue_standalone` with the original input, warn on its failure — D12 swallow-all; `Err` exists only on the in-tx variant).

**`aggregate_login_failure_buckets(window_secs)`** — concrete two-statement shape (uuid v5 computed in Rust; `uuid = "1.18.1"` with `v5` feature already at root `Cargo.toml:110`; **no `uuid-ossp`/`pgcrypto` dependency introduced**, no offline-data rebuild):
```
SELECT floor(extract(epoch FROM created_at) / $1)::bigint * $1 AS bucket_start, count(*) AS n
  FROM login_failures
 WHERE (floor(extract(epoch FROM created_at) / $1)::bigint + 1) * $1 <= extract(epoch FROM clock_timestamp()) - $1
 GROUP BY bucket_start ORDER BY bucket_start;          -- closed buckets only (bucket_end <= now - window)
-- per bucket (loop; each idempotent):
INSERT INTO audit_governance_outbox (event_id, status, class, priority, payload)
VALUES ($v5, 0, 'message', 10, <envelope>) ON CONFLICT (event_id) DO NOTHING;
-- event_id = Uuid::new_v5(NAMESPACE_URL, "aero.im.audit.l1:{nil-workspace}:auth.login.failure:{bucket_start_epoch}")
-- envelope: governance_envelope(..., action "auth.login.failure", outbound "admin.auth.login.failure",
--   outcome "failure", source_system "aero-auth", detail {"count":N,"window_start_epoch":…,"window_secs":…})
--   + top-level "aggregated": true
```
Constraints: no `audit_events` row ever (D7 — avoids the 0236 v1 trigger side effect and default-workspace audit-view pollution); timer never deletes base rows (forensic retention); concurrent multi-instance safe (ON CONFLICT); rerun idempotent.

**`FailedPairRepo`** (new file):
```rust
pub struct FailedPairRepo { pool: PgPool }
impl FailedPairRepo {
    pub fn new(pool: PgPool) -> Self;
    /// Same-tx, after ROLLBACK TO SAVEPOINT (Ea). Returns the DLQ row id.
    pub async fn enqueue_in_tx(tx: &mut Transaction<'_, Postgres>, workspace: WorkspaceId,
        actor: Option<ParticipantId>, action: &str, target: Option<&str>,
        detail: serde_json::Value, outbound_action: &str,
        error_sqlstate: &str, error_message: Option<&str>) -> Result<i64, sqlx::Error>;
    /// Best-effort fresh-tx enqueue for record_pair_standalone's Err branch.
    pub async fn enqueue_standalone(/* same params */) -> Result<i64, sqlx::Error>;
    pub async fn count(&self) -> Result<i64, sqlx::Error>;                       // gauge sampler
    pub async fn replay(&self, id: i64) -> Result<Option<AuditId>, sqlx::Error>; // rebuilds pair w/ NEW AuditId (D9)
    pub async fn replay_all(&self, limit: i64) -> Result<usize, sqlx::Error>;    // ops loop; no CLI/route consumer in this slice
}
```
`replay`: begin → re-run `append_pair_in_tx_fail_open` with the stored original input → on `Some(audit_id)` `UPDATE audit_governance_failed_pairs SET replayed_at = now()` → commit. A re-failed replay re-enqueues a new DLQ row (at-least-once, acceptable) and leaves the original row for the next ops loop.

**`_in_tx` repository variants** (existing pool methods unchanged — they become thin wrappers: begin → `_in_tx` → legacy `AuditRepo::append_in_tx` → commit, so **all existing db_tests pass verbatim**; the new pair is added only on route-orchestrated paths):
- `PatRepo::create_in_tx(tx, participant, token_hash, name, scopes, expires_at) -> Result<PatId, sqlx::Error>` — existing create body minus begin/commit/audit.
- `PatRepo::revoke_in_tx(tx, id, participant) -> Result<bool, sqlx::Error>` — `revoked_at IS NULL` predicate + rows-affected semantics unchanged; no-op revokes audit nothing.
- `TotpRepo::upsert_secret_in_tx(tx, participant, secret) -> Result<(), sqlx::Error>` — enroll always writes (upsert) → pair always.
- `TotpRepo::activate_in_tx(tx, participant) -> Result<bool, sqlx::Error>` — pair only when `rows_affected > 0` (no-op activates audit nothing).
- `SessionRepo::revoke_and_blacklist_in_tx(tx, id, participant) -> Result<bool, sqlx::Error>` + `revoke_by_hash_in_tx(tx, participant, token_hash) -> Result<bool, sqlx::Error>` (logout) — generic-executor pattern mirroring `AuditRepo::append_on` (`audit.rs:140`) over the :499 CTE.

**`NewRegistration`** gains:
```rust
pub auth_audit: Option<NewRegistrationAudit>,
pub struct NewRegistrationAudit {
    pub action: &'static str,          // "auth.register"
    pub target: Option<String>,        // new participant id
    pub detail: serde_json::Value,     // {} (per §2.5; see D-N1)
    pub outbound_action: &'static str, // "admin.auth.register"
}
```
`RegistrationRepo::create`: when `auth_audit.is_some()` → **replace** the legacy `AuditRepo::append_in_tx` (registration.rs :121) with `append_pair_in_tx_fail_open` (after domain rows, before commit; `Ok(None)` continues silently, `Err` propagates — exactly 1 audit row + 1 outbox row, no double-audit); when `None` → legacy append unchanged. Constructors: registration.rs :177/:240/:361 → `None`; `register_enrolled` (service.rs :234) → `Some`.

### 2.3 aero-auth

- **`audit_tokens.rs`** (single Rust source, mirror + pin test): `AUTH_PAT_CREATE` → `AUTH_PAT_ISSUE = "auth.pat.issue"`; `AUTH_TOTP_ENABLED`/`AUTH_TOTP_DISABLED` → `AUTH_TOTP_ENROLL = "auth.totp.enroll"`; add `AUTH_LOGIN = "auth.login"`, `AUTH_REFRESH = "auth.refresh"`, `SESSION_REVOKED = "session.revoked"` (moved **into** the vocabulary), `SESSION_REVOKED_ADMIN = "session.revoked.admin"`. `SERVER_OWNED` shrinks to `["auth.login.new_ip", "auth.login.recovery_code"]`. Pin test `auth_tokens_are_pinned_verbatim` + disjoint test updated in the same commit (they force the mirror).
- **`register_enrolled`** (:222): `auth_audit: Some(NewRegistrationAudit { action: AUTH_REGISTER, target: Some(participant_id.to_string()), detail: json!({}), outbound_action: OUTBOUND_AUTH_REGISTER })`.
- **`refresh`** (:399): after successful validation, before returning: `AuditGovernanceOutboxRepo::new(self.repo.pool().clone()).record_pair_standalone(WorkspaceId::from_uuid(uuid::Uuid::nil()), Some(pid), AUTH_REFRESH, Some(session_id.to_string()), json!({}), OUTBOUND_AUTH_REFRESH).await` — fail-open, never flips the refresh result (aero-auth has no `DEFAULT_WORKSPACE_ID` const; use the `session.rs:221` spelling).

### 2.4 aero-server

| Route | Change |
|---|---|
| `POST /api/auth/login` (`handlers/auth.rs`) | after `record_session` (:201), next to `record_login_event` (:208): `record_pair_standalone(DEFAULT_WORKSPACE_ID, Some(pid), AUTH_LOGIN, Some(pid.to_string()), json!({}), OUTBOUND_AUTH_LOGIN)`. **D5 comment pinned at the call site**: "must never move into `AuthService::login` — a 2FA-failed attempt would be recorded as a successful login". `auth.login.new_ip` / `auth.login.recovery_code` untouched. 2FA-failure paths never emit `auth.login`. |
| `POST /api/pat` / `DELETE /api/pat/:id` (`pat.rs` :59/:117, :60/:162) | switch to begin → `create_in_tx`/`revoke_in_tx` → `append_pair_in_tx_fail_open` (pair: `AUTH_PAT_ISSUE`/`AUTH_PAT_REVOKE`, target=pat_id, detail `{"scopes":[…]}` — **no free-text name**, §2.5) → commit. Revoke idempotency unchanged (no-op revokes audit nothing). |
| `POST /api/me/2fa/enroll` / `verify` (`twofa.rs` :94/:133) | begin → `upsert_secret_in_tx`/`activate_in_tx` → pair (`AUTH_TOTP_ENROLL`, target=participant_id, detail `{"stage":"enroll"\|"activate"}`) → commit; pair only when a row flipped (activate no-op audits nothing). |
| `DELETE /api/auth/sessions/:sid` (`sessions.rs` :91) | begin → `revoke_and_blacklist_in_tx` → pair (`SESSION_REVOKED`, target=session_id, detail `{"session_id":…}` — **already the current shape** at `sessions.rs:129`) → commit; **delete** pool-level `audit_session_revoked` (:108/:114-135). |
| `POST /api/auth/logout` (`session.rs` :40) | begin → `revoke_by_hash_in_tx` → pair (`SESSION_REVOKED`, target=session_id, detail `{"session_id":…}` — **shape change** from `{"token_hash_prefix":…}` at :220-236, see D-N2) → commit; **delete** pool-level `audit_session_revoked` (:212/:220-236). |
| admin force-revoke | **storage-side only** (`auth_session/admin_revoke.rs:112`): replace the `append_in_tx` with `append_pair_in_tx_fail_open` — `SESSION_REVOKED_ADMIN`, target = revoked participant id, detail `{"admin_revoked": true}` — **route zero-change**. Workspace = the existing `workspace` arg (target's/revoker's workspace). |
| L1 timer (`bin/boot/background.rs`) | new ticker: env `AERO__SERVER__LOGIN_FAILURE_L1_AGGREGATE_SECS` (default 30, 0 disables; env-only like `LOGIN_FAILURE_RETENTION_DAYS` — no config.toml key), `MissedTickBehavior::Skip`, shared cancel token, best-effort warn, calls `aggregate_login_failure_buckets`. Boot-time `tracing::warn!` once when env=0 and `AERO__SERVER__LOGIN_FAILURE_RETENTION_DAYS > 0` (D11/F15). Timer never deletes base rows. |
| gauge sampler (`bin/boot/metrics_tasks.rs`) | 30s ticker mirroring the AI DLQ-depth pattern (existing 30s samplers at :142/:174/:208): `FailedPairRepo::count` → `aero_audit_governance_failed_pairs`. |

### 2.5 Observability (D10 — all mandatory)

- Counter **`audit_auth_write_failures_total{category ∈ {contract, binding, database}}`** — const `AUDIT_AUTH_WRITE_FAILURES_TOTAL` into `aero-common/src/metrics.rs` `names` + `register_known_metrics` (:791) with help; SQLSTATE is the classification key; emit point is the shared storage write path (`append_pair_in_tx_fail_open` — storage's **first** metric, deliberate exception; the counter is a process-global registry static). Log lines carry `workspace_id`, `action`, `path` (`in_tx`\|`standalone`), `error_sqlstate`, DLQ row id (post-enqueue); **detail never logged** (only enters the DLQ table). Levels: P0001/binding + contract-bug classes (23514/23503/23505/other) → ERROR + count; transient (40P01/40001/55P03) → warn + count; contract pre-check → warn + count; connection-level → propagate (in-tx) / warn (standalone), uncounted.
- Gauge **`aero_audit_governance_failed_pairs`** — DLQ depth, 30s sampler (above).
- Naming note (D-N3): the counter literal `audit_auth_write_failures_total` lacks the `aero_` namespace prefix the `names` module doc prescribes — kept verbatim per the governing design (both designs agree); flag for the connector design review, do not silently "fix".

### 2.6 Harness

- `scripts/test-integration.sh`:
  - B5-1 segment (:317-336): add `run_migrated_integration "$AUTH_OUTBOX_PARITY_INTEGRATION_DB" "auth_outbox_parity" "auth outbox parity"` inside the 0239-file gate + its `b5_check "auth_outbox_parity" "SKIP (0239 not landed)"` twin in the else branch (:339-342).
  - **F-4**: static arbiter :620 `MIGRATION_COUNT -ne 244` → `-ne 246` in the same commit as the migrations.
- `scripts/b5-pin.sh`: add `auth_outbox_parity` to `B5_CONTRACT_TEST_LIST` (executed slot), 40 → 41; guard :91 `count -ne 40` → `-ne 41`; header comment 19+21 → 20+21 (F-2).
- `scripts/test-b5-pin-guard.sh`: fabricated-evidence fixture (:39) + assertions (:64, :114) "40/40 (19 executed, 21 [PROPOSED])" → "41/41 (20 executed, 21 [PROPOSED])" in the same commit.
- `aero-audit-t11-drill` binary + `t11-fail-closed` slot: **PASS unchanged** — its seed `action: "auth.login.failed"` (class message, priority 10) is outside this slice's allowlist and must not be modified.

---

## 3. Compatibility constraints

1. **Verbatim-unchanged**: `migrations/0239/0240/0241/0242/0245/0246` DDL; `crates/aero-ai/src/governance.rs` mapping authority (D1 — auth rows are written explicitly, never by trigger-token extension); existing `aero_common::model::audit` leaf constants (`L1_WINDOW_SECONDS` etc. — only **new** additive constants are allowed, e.g. none needed at the leaf: tokens live in aero-storage per D1); every existing test name/literal in `audit_governance/db_tests/`; `crates/aero-audit-connector` (zero-change); the 0236 v1 path; `auth.login.new_ip`/`auth.login.recovery_code` best-effort producers; `LoginFailureRepo::record`; login throttle/`finalize_login` semantics; `message.moderated`'s admin/100 lane; GDPR export behavior.
2. **Pair atomicity (R1)**: 1 audit row + 1 outbox row (`event_id = audit_events.id` 1:1, `status=0`, class/priority per §2.7) in the same tx; fail-open allows only "whole pair absent" (R7), never half-pairs; every non-success branch leaves zero written rows and the original input is re-parkable (DLQ — at-least-once discipline, AGENTS.md §4.2).
3. **Auth rows unconditionally enqueued (R6)**: the pair writer has **no** `snaplink_commercial_runtime.enabled` gate, no binding lookup, no RAISE of its own — the only enforcement interaction is the 0236 v1 trigger's side effect on the audit row (v1/v2 coexistence unchanged; pinned by the F1 regression pair).
4. **No new dependencies**: uuid v5 already at root `Cargo.toml:110`; runtime sqlx queries only (no offline-data rebuild).
5. **Existing public repo APIs unchanged**: pool-level `PatRepo::{create,revoke}`, `TotpRepo::{upsert_secret,activate,disable}`, `SessionRepo::revoke_and_blacklist` keep signatures/behavior (wrappers over `_in_tx` + legacy audit); routes are the only paths gaining pairs. `NewRegistration` gains one field — all constructors updated in the same commit (compile-forced).
6. **Token renames are a vocabulary break** (DP-1): `auth.pat.create` → `auth.pat.issue`, `auth.totp.enabled`/`auth.totp.disabled` → `auth.totp.enroll`. Verified consumers: only the storage emitters, `audit_tokens.rs`, and storage db_tests (pat.rs :381-411, totp.rs :531+) — all updated in the same commit; the connector t11 seed uses `auth.login.failed` (untouched).
7. **Migration numbering fixed (D8/DP-2)**: 0243 + 0244 unconditionally; pre-write check `ls migrations/*.sql | grep -cE '^(0242|0243|0244)_'` ≤ 1 (expect exactly 1 = 0242). Harness arbiter 244 → 246 same-commit (F-4).
8. **L1 lane discipline (D2 outcome A / DP-3)**: auth L1 rows are class `message`, priority 10 — they share the 0240 backlog claim lane with message backlog (`priority DESC` — moderation 100 first; auth never preempts); `admin_class_rows_never_aggregated` (:238 test + the `is_admin_class` guard) untouched; the aggregation row itself is message-domain.
9. **Working-tree hygiene (sequencing)**: the in-flight B5-3 connector/harness edits (F-5) must be committed first; this slice only touches `scripts/test-integration.sh` among the in-flight files — merge cleanly on top.
10. **IMMEDIATE-constraint pin (D4)**: a comment in 0244 DDL + the `audit_governance/mod.rs` doc note: 0239/0236 CHECK/RAISE are all IMMEDIATE; a future DEFERRABLE change breaks the SAVEPOINT branch (whole-tx fail-closed).

---

## 4. Failure modes

| # | Failure | Detection | Handling | Outcome / invariants |
|---|---|---|---|---|
| F-1 | Non-object `detail` (contract bug) | Rust pre-check | warn + `counter{contract}` | `Ok(None)`, 0 rows, no DB touch, domain commits |
| F-2 | Binding RAISE `P0001` (0236 v1 on the audit row: enforcement on + no enabled binding) | `sqlx::Error::Database` | SAVEPOINT rollback → DLQ same-tx → ERROR + `counter{binding}` | `Ok(None)`, 0 rows, domain commits, **exactly 1 DLQ row** (`error_sqlstate='P0001'`) |
| F-3 | Contract-bug classes 23514/23503/23505 (incl. event_id 23505 collision) | Database | SAVEPOINT rollback → DLQ → ERROR + `counter{contract}` | whole pair gone (never single row — F4-finalized), domain commits |
| F-4 | Transient 40P01/40001/55P03 | Database | SAVEPOINT rollback → DLQ → warn + `counter{database}` | `Ok(None)` (savepoints recover from deadlock/serialization) |
| F-5 | Connection-level (IO/Protocol/PoolTimedOut/Closed) | non-Database (`is_fail_open_error` false) | ROLLBACK TO SAVEPOINT → **propagate** (in-tx) / rollback+warn+best-effort standalone DLQ → `None` (standalone) | in-tx: domain op fails as today (never half-pair, never silent drop); standalone: login/refresh succeed, pair lost only if the best-effort DLQ also fails (warn) |
| F-6 | DLQ enqueue fails inside the pair tx (e.g. 42P01 — 0244 missing from a bad deploy) | Database on DLQ insert | nested `SAVEPOINT aero_audit_dlq` → ROLLBACK TO → ERROR log (no detail) | still `Ok(None)` — domain commits; pair lost-but-logged (fail-open preserved; R7 not broken) |
| F-7 | L1 timer disabled × retention sweep | boot `tracing::warn!` (env=0 && retention>0) | accepted signal loss for that window (D11) | reopening the timer within the retention window self-heals (pull-scan backfill) |
| F-8 | Concurrent multi-instance L1 aggregators | — | `ON CONFLICT (event_id) DO NOTHING` (deterministic v5) | exactly 1 row per closed bucket, counts stable |
| F-9 | Replay of a still-failing input | Database inside replay's pair write | fail-open again → new DLQ row; original row kept (`replayed_at` only set on success) | at-least-once, no row loss, no infinite retry spin (ops loop drives) |
| F-10 | Login-producer placement regression (auth.login before 2FA gate) | AC-2 login leg + D5 comment | — | 2FA-failed attempts must never emit `auth.login` (load-bearing, pinned) |
| F-11 | `auth.login`/`auth.refresh` emit with `workspace` non-nil | nil-workspace constant at call sites | account-level events land in the nil tenant (E18) | audit-view partitioning unchanged |

**Fail-open guardrail (risk §5)**: fail-open must never become fail-closed — every non-success branch either rolls back the whole pair (SAVEPOINT) or enqueues the DLQ row with the same fate as the domain tx (F-6 nested savepoint is the last-resort guard).

---

## 5. Migration steps (ordered)

1. **Pre-flight**: commit/stash the in-flight B5-3 batch (F-5); `git status` clean of unrelated in-flight work. Pre-write check: `ls migrations/*.sql | grep -cE '^(0242|0243|0244)_'` → exactly 1 (0242).
2. **Write** `migrations/0243_login_failures_created_at_idx.sql` + `migrations/0244_audit_governance_failed_pairs.sql` (DDL per §2.1; header comments per D8 — 0243 notes the renumbering from the original 0242 plan; 0244 carries the IMMEDIATE-constraint pin).
3. **`cargo build`** (migrations compile into the bin via `sqlx::migrate!("../../migrations")`, `aero-storage/db.rs`) → **`aero-cli migrate`** on a throwaway DB. Both must land in the same batch (0244 is the hard prerequisite of the fail-open path — a missing table would surface as 42P01 after SAVEPOINT rollback; the nested-savepoint guard in F-6 bounds the damage but R7's DLQ compensation is lost). Verify `_sqlx_migrations` contains 243 and 244; `ls migrations/*.sql | wc -l` = 246.
4. **Same commit as the migrations**: flip the harness arbiter `MIGRATION_COUNT` 244 → 246 (F-4, `scripts/test-integration.sh` :620).
5. **aero-storage**: `tokens.rs` + `outbox.rs` + `failed_pairs.rs` (repo, envelope, fail-open pair, standalone pair, L1 aggregation, `is_fail_open_error`, `FailedPairRepo`) + `SnaplinkCommercialRepo::has_enabled_binding` (designed-only per connector design §2.1 — implement with the F1 fixture as its first consumer) + `_in_tx` variants (pat/totp/auth_session) + `NewRegistration.auth_audit` + token renames in emitters + all db_tests (AC-2/AC-3/AC-5 + F1 regression pair + G1 unit). Existing parity names/literals untouched.
6. **aero-auth**: `audit_tokens.rs` rename/new consts + pin-test update; `register_enrolled` `Some`; `refresh` write point.
7. **aero-server**: login handler point (D5 comment), PAT/TOTP/sessions/logout tx orchestration, remove pool-level post-hoc session-revoked audits, admin-revoke pair (storage-side, route zero-change), L1 boot timer + disable-warn, metrics (names + `register_known_metrics` + counter + gauge sampler).
8. **Harness**: `test-integration.sh` entry + SKIP twin + arbiter; `b5-pin.sh` 41 slots; `test-b5-pin-guard.sh` 41/41 assertion.
9. **Gates** (AGENTS.md §4.3): `cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets` (no new warnings) · `scripts/{truth-check,file-size-check,web-check}.sh` 0 violations · `scripts/test-integration.sh` B5 segment all PASS (no SKIP on `auth_outbox_parity`) · `scripts/test-b5-pin-guard.sh` green · PG-gated `-- --ignored --test-threads=1` on a disposable DB, then `DROP DATABASE`.

---

## 6. Testable acceptance mapping

| Acceptance (req doc §5) | Testable artifact | Location / name |
|---|---|---|
| AC-1 migrations land, count 246 | `ls` + fresh-DB `aero-cli migrate` + `_sqlx_migrations` 243/244 + chain replay; **arbiter 244→246 (F-4)** | `scripts/migrate_chain_smoke.sh` semantics; `scripts/test-integration.sh` :613-621 |
| AC-2 §2.7 parity + per-token 1:1 | `auth_outbox_parity_1to1` — for **each** token (`auth.register`, `auth.login`, `auth.refresh`, `auth.pat.issue`, `auth.pat.revoke`, `auth.totp.enroll`, `session.revoked`, `session.revoked.admin`): drive the **actual route-shaped write path** (`append_pair_in_tx_fail_open` / `_in_tx` variants) → exactly 1 outbox row, `event_id = audit_events.id`, `status=0`, `class='admin'`, `priority=10`, 16-key envelope field-by-field recomputed from leaf consts (never inline literals — lanes.rs G2-closure pattern); commit-half → N rows, rollback-half → 0 (whole pair), replay-half → deduped. **Named harness entry `auth_outbox_parity`** (empty-filter guard makes the name load-bearing). L1 companion: `login_failure_l1_aggregation_n_to_one` — seed 3 same-bucket + 2 different-bucket rows (window=60) → exactly 2 rows (count 3/2), class message, status 0, deterministic v5 event_id, `payload->>'aggregated'='true'` at envelope top level, rerun idempotent, base-row rollback → 0 | `crates/aero-storage/src/audit_governance/db_tests/auth.rs` + `l1_auth.rs` (≤800-line split), `#[ignore = "requires live Postgres"]` |
| AC-3 no-miss-write + F1 regression pair | `auth_allowlist_no_miss_write` (named filter): enforcement-on + binding fixture (`enable_enforcement_with_binding`, db_tests.rs:96) → exercise every producer → the verbatim allowlist `NOT EXISTS` query = 0. `auth_pair_enforcement_on_with_nil_binding_commits_pair_and_v1` — pair 1+1 **and** 1 0236 v1 `snaplink_delivery_outbox` row. `auth_pair_enforcement_on_without_nil_binding_fails_open_drops_pair` — fixture direct flip (control-plane-unreachable state) → `Ok(None)`, 0 rows, domain commits, **exactly 1 DLQ row** (`error_sqlstate='P0001'`), `audit_auth_write_failures_total{category="binding"}` ≥ 1 (process-global, `>=` semantics). Both restore via `restore_enforcement_disabled` (:145) | `db_tests/auth.rs` (F1 pair) + `db_tests/failed_pairs.rs` |
| G1 unit (propagation branch) | `is_fail_open_error` classifier: non-Database errors → false (propagate/warn) | unit test in `outbox.rs` (no PG) |
| AC-4 harness pin growth / t11 stays PASS | `auth_outbox_parity` verdict line in B5 log; b5-pin 40→41 (19→20 executed); pin-guard "41/41 (20 executed, 21 [PROPOSED])" same commit; t11 drill + `t11-fail-closed` slot PASS unchanged | `scripts/{test-integration.sh,b5-pin.sh,test-b5-pin-guard.sh}`; `aero-audit-t11-drill.rs` untouched |
| AC-5 fresh-chain no outbox-less allowlist row | `run_migrated_integration` legs `audit_governance::` + `auth_outbox_parity` + `auth_allowlist_no_miss_write` on fresh throwaway DBs migrated from the full chain (empty-filter guard applies — vacuous green FAILs) | `scripts/test-integration.sh` B5-1 segment |
| R6 observability | counter registered in `aero-common` `names` + `register_known_metrics`; gauge sampler 30s; F1 test asserts counter increment; DLQ depth gauge wired | `metrics.rs`, `bin/boot/metrics_tasks.rs`, db_tests |
| D5 login placement | AC-2 login leg + call-site comment; 2FA-failure path asserts no `auth.login` row | `handlers/auth.rs` + `db_tests/auth.rs` |
| Harness regression (F-4) | `MIGRATION_COUNT` arbiter = 246 | `test-integration.sh` :620 |

---

## 7. Decision points resolved

- **DP-1 (tokens — resolved to §2.7, hard constraint)**: `auth.pat.create` → `auth.pat.issue`; `auth.totp.enabled`/`auth.totp.disabled` → `auth.totp.enroll`. Keeping the old literals makes AC-2/AC-3 unsatisfiable (the allowlist tokens would never be produced). `session.revoked` kept (D6); `session.revoked.admin` new.
- **DP-2 (numbering — fixed)**: 0243 + 0244 unconditionally; 0242 stays aero-ai's.
- **DP-3 (L1 class — fixed, D2 outcome A)**: class `message`, priority 10.
- **DP-4 (standalone signature — fixed, D12)**: `Option<AuditId>`, swallow-all; `Err` only on the in-tx variant.
- **D-N1 (register detail — new)**: §2.7 pins detail `{}`, which **changes** the existing `auth.register` audit row's detail from `{"email":…,"user_agent":…}` to `{}` (PII reduction — email leaves the audit trail; deliberate per design §2.5). Flag: any consumer of the register audit detail must be re-checked (none found in repo).
- **D-N2 (session.revoked detail shape — new)**: inventory revoke already writes `{"session_id":…}` (sessions.rs:129); **logout** changes from `{"token_hash_prefix":…}` (session.rs:230) to `{"session_id":…}` — trajectory shape change, token string kept (D6). No in-repo consumers of `token_hash_prefix` found.
- **D-N3 (metric naming)**: `audit_auth_write_failures_total` lacks the `aero_` prefix — kept verbatim per governing design; flagged for the connector design review.
- **D-N4 (SSO/OIDC JIT)**: `register_enrolled` gets the pair; the 3 other `NewRegistration` constructors (SSO JIT etc., registration.rs :177/:240/:361) pass `None` → `auth.register` rows from JIT stay audit-only (no outbox). AC-3's query is safe on test DBs; a production no-miss-write monitor must scope to `detail.provisioning IS NULL` or JIT must gain pairs in a follow-up (one-line `Some`).

## 8. Risks

1. Prior batch not committed (F-5) — sequencing gate; clobbering `scripts/test-integration.sh` B5-3 edits.
2. 0243/0244 not in one build→migrate batch → 42P01 on DLQ (bounded by F-6 nested savepoint, but R7 compensation lost).
3. Login point drifting into `AuthService::login` → 2FA-failed attempts recorded as successful logins (D5; comment + AC-2 leg).
4. Auth rows at priority 10 share the claim lane with message backlog — ordering `priority DESC` (moderation 100 first); no lane filtering (0240 index).
5. `audit_governance::` filter already matches existing tests — the **named** `auth_outbox_parity` + `auth_allowlist_no_miss_write` entries (empty-filter guarded) prevent vacuous green.
6. L1 disable × retention sweep loses that window's v2 signal (accepted, D11) — boot warns; reopen within retention self-heals.

---

## 9. design-resolved — design_gate blockers (VERDICT: FAIL, 2026-08-14; run 386d075c)

The design gate verified the landed implementation (committed with the review
stage as 5ac8a90: migrations 0243/0244, `AuditGovernanceOutboxRepo` H3 pair
writer, failed-pairs DLQ, DP-1 token renames, L1 aggregation) against the
design and found 8 BLOCKING findings, all verified in-tree. Resolutions
(carry explicit reviewer fix prescriptions):

| # | Finding (severity) | Verified anchor | Resolution (fix prescription) |
|---|---|---|---|
| **C1** (protocol, BLOCKING) | `source_system="aero-auth"` dead-letters the whole auth lane | `client.rs:518-519` `validate_delivery_payload` bails on `source_system != config.source_system`; `config.rs:121` reads `AERO_AUDIT_SOURCE_SYSTEM` (default `aero-im.source`, :300); 0242 header pins `aero-im.source` | **One-const change**: auth emitter `AUTH_SOURCE_SYSTEM = AUDIT_SOURCE_SYSTEM` (the leaf `aero_common::AUDIT_SOURCE_SYSTEM`); the auth pair + L1 rows then survive the connector guard. Parity tests must assert against the leaf const, not the local string |
| **F-A** (perf, BLOCKING) | L1 scan non-sargable: `WHERE (floor(extract(epoch FROM created_at)/$1)::bigint + 1) * $1 <= extract(epoch FROM clock_timestamp()) - $1` — full Seq Scan of the 180-day window every tick; 0243 index unusable; no watermark | `outbox.rs:~417` | Rewrite to sargable form on `created_at` (e.g. `created_at <= now() - make_interval(secs => $1)`), leveraging the 0243 `login_failures_created_at_idx`; carry a per-run watermark (max processed `created_at`) so each tick scans only new rows |
| **C2** (protocol, BLOCKING) | L1 window = tick env `AERO__SERVER__LOGIN_FAILURE_L1_AGGREGATE_SECS` (default 30) ≠ `L1_WINDOW_SECONDS` (60) → prod keys ≠ test keys; ops env change re-keys buckets → sink double-counts | `background.rs:317/:361` | Single call site: the aggregation uses the leaf `L1_WINDOW_SECONDS` constant; drop the env knob (or keep it only as a disable switch, 0 = off), never as a window-size override |
| **F-2** (security, BLOCKING) | TOTP disable emits `auth.totp.enroll` — 2FA *removal* recorded as *enrollment* (audit-only; production AC-3 false-positive class) | `totp.rs:206-214` | Restore the distinct `auth.totp.disabled` token for the disable path (DP-1 rename half kept for the enable path only: `auth.totp.enabled` → `auth.totp.enroll`) |
| **F-3** (security, BLOCKING) | `complete_oidc_login` (`sso.rs:714-770`) issues tokens + records session with zero audit — no `auth.login`, no pair, no pin | `sso.rs:714-770` | Emit the `auth.login` pair at `complete_oidc_login` in the same tx as token issuance (D5 placement pattern of `handlers/auth.rs:204`); add the SSO pin to authz_lint |
| **F-4** (security, BLOCKING) | `failed_pairs.rs` has no `replay_attempts` cap, no dead state, no retention; `replay_all` doubles the table per run under a persistent bug | `failed_pairs.rs` | Add `replay_attempts` (bounded, MAX_ATTEMPTS=5 → `dead` state), retention sweep for dead rows, and a `replay_all` guard (cap per run) |
| **F-1** (security, BLOCKING) | `record_pair_standalone` connection-level branches (begin :317-325, commit :333-341, connection :371-376) warn-only, no `count_failure` — the only attacker-inducible class is uncounted | `outbox.rs:317-376` | Count every connection-level failure with `count_failure` (the `audit_auth_write_failures_total` metric) so the alert control plane sees the attacker-inducible class |
| **QA F-1/F-2** (HIGH) | `authz_lint.rs` has zero B5-1 rules (no `_in_tx` reference checks, no `audit_session_revoked`-gone check, no D5/SSO placement pins); no HTTP-level smoke | `crates/aero-server/tests/authz_lint.rs` | Add B5-1 authz_lint rules: `_in_tx` references required on PAT/TOTP/session writes, `audit_session_revoked` removal pin, D5 login-handler placement oracle, SSO `complete_oidc_login` pair placement; add the HTTP-level smoke leg |

**Re-verification path**: implement stage lands the 8 fixes in this order:
C1 (one-const) → C2 (single window site) → F-A (sargable + watermark) →
F-2 (totp.disabled) → F-3 (SSO pair) → F-4 (replay caps) → F-1 (counting) →
QA (authz_lint rules + smoke) → full gates (`cargo check`, `test-integration.sh`,
b5-pin 41/41, clippy no-new-warnings).

**Non-blocking residuals** (recorded, not in this batch's acceptance): F-B
(outbox-terminal/DLQ sweep), F-C (clock-domain freeze + `ON CONFLICT DO
NOTHING` merge arm), F-E (0243 header claim), F-F, C3 (`Z` vs `+00:00`),
C4 (two L1 envelope shapes), C8 (payload unprojected), security F-5/F-6
(provisioning carve-out pin; `credential_rotation.rs:69/:167` audit-only).
