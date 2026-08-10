# Requirements Specification — Carry PAT scopes through verification + fail-closed `require_scope` guard

- **Module**: `crates/aero-auth/src` (+ minimal touches in `crates/aero-storage/src/pat.rs`, `crates/aero-server/src/pat.rs`)
- **Direction**: "Carry PAT scopes through verification and add a fail-closed require_scope guard (B5-4 enforcement point)"
- **Source analysis**: `docs/auto/analyses/crates-aero-auth-src-650f2e56.json` (direction #1 of 3)
- **Date**: 2026-08-08
- **Status**: Spec — every citation below re-verified against the repo on this date

---

## 1. Problem statement (evidence-verified)

B5-4 requires that `audit:event:write` be granted **only after the relay works** and that the system **fail closed** without it. Today the scope is minted, normalized, persisted, and listed — but **dropped at every authenticated request**:

1. `pat_tokens.scopes` is written at mint (`aero-storage/src/pat.rs:114` INSERT) and listed (`pat.rs:175`), but `PatRepo::verify` selects only `participant_id` (`pat.rs:139`). The scope is **never read back** during authentication.
2. `PatVerifier::verify` returns only `Option<ParticipantId>` (`aero-auth/src/pat.rs:30`; blanket impl `pat.rs:38-39`).
3. `AuthUser` carries `{ participant_id, session_id, exp }` and nothing else (`aero-auth/src/extractor.rs:34-40`).
4. **No scope check exists anywhere** in `aero-auth`/`aero-server`: `rg require_scope` → exit 1, zero hits. The only in-repo scope-check precedent is OIDC client-credentials validation (`aero-auth/src/oidc.rs:378-481`).
5. `SCOPE_AUDIT = "audit:event:write"` (`aero-audit-connector/src/client.rs:43`) is used **outbound only** (token acquisition + pre-POST claim validation). Nothing gates an inbound caller presenting an audit-scoped PAT.

Result: a minted `audit:event:write` PAT is indistinguishable at request time from a scope-less PAT — the grant is unenforceable, so B5-4's fail-closed posture cannot be expressed.

### 1.1 Evidence verification table

| Analysis citation | Verified anchor (this repo, 2026-08-08) | Status |
|---|---|---|
| `aero-auth/src/pat.rs:13-16` — `PatVerifier::verify` drops scopes | Trait `async fn verify(&self, token_hash: &str) -> Option<ParticipantId>` at `pat.rs:30`; blanket impl for `PatRepo` at `pat.rs:38-52` returns owner only | ✅ Confirmed (line drift: 13-16 → 28-30) |
| `aero-auth/src/extractor.rs:32-39` — `AuthUser` has no scopes | `pub struct AuthUser` at `extractor.rs:34-40` with `participant_id` / `session_id` / `exp` only; `#[derive(Debug, Clone, Copy)]` | ✅ Confirmed (drift: 32-39 → 34-40) |
| `aero-auth/src/service.rs verify_pat` — `is_well_formed_pat` gate → verifier | `verify_pat` at `service.rs:434-447`: `pat_verifier.as_ref()?` → `is_well_formed_pat` → `hash_pat` → `verifier.verify(&hash)` → `Option<ParticipantId>` | ✅ Confirmed |
| `aero-storage/src/pat.rs:37,114` — scopes persisted, never read back on verify | `PatSummary.scopes` at `pat.rs:37`; INSERT binds `scopes` at `pat.rs:114`; `verify` SELECT at `pat.rs:139` reads **only** `participant_id`; `scopes TEXT[] NOT NULL DEFAULT '{}'` in `migrations/0019_pat.sql:23` | ✅ Confirmed |
| `aero-auth/src/oidc.rs:367-455` — scope-check precedent to mirror | `ClientCredentialsTokenConfig.required_scopes` at `oidc.rs:378-381`; `ClientCredentialsClaims::granted_scopes()` BTreeSet union at `oidc.rs:401-408`; enforced in `validate_client_credentials_token` at `oidc.rs:478` (`required_scopes.iter()` all-present check) | ✅ Confirmed (drift: 367-455 → 378-481) |
| `aero-audit-connector/src/client.rs:38` — `SCOPE_AUDIT`, outbound only | `pub const SCOPE_AUDIT: &str = "audit:event:write"` at `client.rs:43`; used in token acquisition (`client.rs:426` scope param) and claim validation (`client.rs:289-292` expected_scope) — both outbound legs | ✅ Confirmed (drift: 38 → 43) |
| `aero-eng/src/audit_provision.rs Q0_SQL` — relay-health predicate | `Q0_SQL` at `audit_provision.rs:34`: `SELECT runtime.enabled, (SELECT count(*) FROM snaplink_commercial_bindings WHERE enabled) FROM snaplink_commercial_runtime runtime WHERE runtime.singleton`; consumed at `audit_provision.rs:536-543` | ✅ Confirmed |
| Provisioning substrate | `migrations/0235_snaplink_commercial_control_plane.sql:8-16`: `snaplink_commercial_runtime(singleton PK CHECK, enabled BOOLEAN NOT NULL DEFAULT FALSE)` + singleton row seeded `FALSE`; `snaplink_commercial_bindings` at `0235:18-27` (`enabled BOOLEAN NOT NULL`) | ✅ Confirmed |
| No scope check exists | `rg -n require_scope crates/aero-auth/src crates/aero-server/src` → no matches (exit 1); scope mentions in aero-auth are confined to `oidc.rs`/`oidc/tests.rs` (client-credentials path) | ✅ Confirmed |
| Mint route shape | `aero-server/src/pat.rs`: `MAX_SCOPES = 64` (:52), request `scopes` field documented "Reserved for future fine-grained authorization; stored, not yet enforced" (:69-72), `normalize_scopes` (:102-115), `mint_pat` (:120-152) calls `PatRepo::create` with normalized scopes | ✅ Confirmed |
| Bot tokens scope-less by design | `aero-auth/src/bot.rs:40`: `async fn verify(&self, token: &str) -> Option<ParticipantId>` — no scope concept | ✅ Confirmed |
| 403-equivalent denial primitive | `aero_common::Error::Forbidden(String)` at `crates/aero-common/src/error.rs:19`; maps to HTTP 403 (`error.rs:70`) and `"forbidden"` code (`error.rs:85`) | ✅ Confirmed |
| Test-style precedents to mirror | `well_formed_pat_*` hermetic tests at `aero-auth/src/service.rs:626-671`; `claim_validation.rs` parameterized assertions in `aero-audit-connector/tests/`; PG-gated `#[ignore]` db_tests in `aero-storage/src/pat.rs:296-360` | ✅ Confirmed |

---

## 2. Requirements

### R1 — Carry PAT scopes through verification (aero-auth + aero-storage)

**R1.1** Change `PatVerifier::verify` (`aero-auth/src/pat.rs:30`) to return a resolution that carries both the owner and the token's scopes, instead of `Option<ParticipantId>`. Introduce `PatResolution` (or equivalent tuple/struct) in `aero-auth/src/pat.rs` with fields:
- `participant_id: ParticipantId`
- `scopes: Vec<String>` (the token's scopes as persisted; `[]` for scope-less tokens)

The "unknown/revoked/expired → `None`, DB error → `None` + `tracing::warn!`" semantics (current `pat.rs:38-52`) are unchanged — a failed lookup must never fabricate scopes.

**R1.2** Change `aero_storage::PatRepo::verify` (`aero-storage/src/pat.rs:137-167`) to select and return the scopes column together with the owner:
- SQL: `SELECT participant_id, scopes FROM pat_tokens WHERE …` (the active-token predicate, deleted-participant EXISTS guard, and best-effort `last_used_at` bump are unchanged).
- `scopes TEXT[] NOT NULL DEFAULT '{}'` (`0019_pat.sql:23`) — no NULL handling required; do not add a `COALESCE` seam.
- Update the blanket impl at `aero-auth/src/pat.rs:38-52` to the new return shape.
- Update `db_tests::pat_create_verify_revoke_cycle` (`aero-storage/src/pat.rs:300-341`) which asserts `repo.verify(…) == Some(owner)` — extend it to also assert the **scopes round-trip** through `verify` (mint with `["read","write"]` → verify returns the same scopes), since the direction's whole point is that the persisted scope survives verification.

**R1.3** Change `AuthService::verify_pat` (`aero-auth/src/service.rs:434-447`) to return the PAT resolution (owner + scopes) instead of `Option<ParticipantId>`. The structural gate (`is_well_formed_pat` before hashing) and the "not wired → `None`" behavior are unchanged.

**R1.4** Add a scopes field to `AuthUser` (`aero-auth/src/extractor.rs:34-40`):
- `scopes: Arc<[String]>` (preferred — `Arc` keeps `AuthUser: Copy`, preserving `extractor.rs:95`'s `parts.extensions.get::<AuthUser>().copied()` and any other `Copy` reliance) **or** `Vec<String>` (then `Copy` is dropped and call sites switch to `.cloned()`/clone — verify no call site requires `Copy`).
- JWT path (`auth_user_from_access_claims`, `extractor.rs:56-68`): scopes = **empty** (JWTs carry no scopes; do not allocate per request — share one static empty value).
- PAT path (`extractor.rs:127-133`): scopes = the resolution's scopes.
- Bot path (`extractor.rs:130-132`): scopes = **empty** (bots are scope-less by design, §4.5).
- The extension stash (`parts.extensions.insert(user.participant_id)`, `extractor.rs:143`) continues to insert the participant id; additionally stash `AuthUser` (already the typed extension read at `extractor.rs:113`) so downstream guards see the scopes without re-decoding.

**R1.5** Update the trait-object wiring: `SharedPatVerifier = Arc<dyn PatVerifier>` (`pat.rs:56`) is unchanged; `lib.rs:33` re-export of `PatVerifier`/`SharedPatVerifier` is unchanged. `PatResolution` must be `Debug + Clone` (unit tests) and `Send + Sync` (trait-object bound).

### R2 — Fail-closed `require_scope` guard (aero-auth)

**R2.1** Add a public fail-closed scope guard reachable from `AuthService` (or as a public free function in `aero-auth`) with signature:

```
pub fn require_scope(user: &AuthUser, scope: &str) -> aero_common::Result<()>
```

Semantics (all fail-closed, never default-open):
- `scope` present in `user.scopes` (exact string match, as-carried — no trimming, no aliasing, no prefix matching) → `Ok(())`.
- `scope` absent (scope-less PAT, JWT, bot token, or a PAT carrying other scopes) → `Err(Error::Forbidden(…))` — the 403-equivalent denial (`error.rs:19,70,85`).
- Unknown/unsupported scope strings are **denied** — the guard grants only what verification carried; it never knows a scope the token did not present.
- Empty/blank `scope` argument → `Err(Error::Forbidden(…))` (deny; do not treat as "no requirement").
- Pure over the carried scopes — **no DB read at guard time**: the single source of truth is the verification-time snapshot from R1. This keeps the guard unit-testable without a pool and avoids a TOCTOU window on `pat_tokens.scopes` (scopes are immutable after mint today; do not introduce a read path).

**R2.2** The guard is the B5-4 enforcement primitive. No inbound audit-ingestion route exists in `aero-server` today (verified — `rg -i audit crates/aero-server/src/routes/routes.rs` → no matches), so **route wiring is out of scope** (§4); the deliverable is the primitive + the mint gate (R3) + the tests that pin its behavior.

### R3 — Mint-time provisioning gate for `audit:event:write` (aero-server)

**R3.1** In `mint_pat` (`aero-server/src/pat.rs:120-152`), after `normalize_scopes` and **before** `PatRepo::create`: if the normalized scopes contain `audit:event:write` (exact string, matching `SCOPE_AUDIT` in `aero-audit-connector/src/client.rs:43`), evaluate the relay-health predicate:
- Predicate SQL mirrors `Q0_SQL` (`aero-eng/src/audit_provision.rs:34`) exactly:
  `SELECT runtime.enabled, (SELECT count(*) FROM snaplink_commercial_bindings WHERE enabled) FROM snaplink_commercial_runtime runtime WHERE runtime.singleton`
- Healthy iff `enabled = TRUE` **and** enabled-bindings count > 0 (the full Q0 tuple — same predicate the provision check treats as the relay-health gate).
- Unhealthy (including a missing runtime row — `WHERE runtime.singleton` returns no row) → reject the mint with `AeroError::Forbidden("audit:event:write requires the audit relay to be provisioned")` → HTTP 403. **No `pat_tokens` row is written** — the gate runs before `create`.
- DB error while evaluating the predicate → fail closed: reject the mint (do not mint an audit-scoped token when health is unknowable).

**R3.2** Scope of the gate: only `audit:event:write` is gated. All other scopes keep today's behavior ("stored, not yet enforced", `pat.rs:69-72`) — an allowlist/denylist for arbitrary scope strings is out of scope (§4).

---

## 3. Acceptance criteria (testable)

### T-11 — Fail-closed posture (hermetic unit tests, `aero-auth/src/service.rs` `#[cfg(test)]`, mirroring `well_formed_pat_*` at `service.rs:627-740`)

Tests are hermetic: `token_service()` (`service.rs:613-627`) uses `connect_lazy` (no live connection needed) and `verify_pat` with an injected fake `PatVerifier` performs no queries (structural gate + hash + trait call). Fake verifier: an `#[async_trait]` impl returning a canned `PatResolution` keyed by token hash.

| Test | Pass predicate |
|---|---|
| `pat_resolution_carries_scopes` | Inject fake returning `PatResolution { participant_id: P, scopes: ["audit:event:write"] }` for the hash of a well-formed minted-shape token (use `aero_storage::pat::generate_pat()`); `AuthService::verify_pat(token)` returns the same `participant_id` **and** `scopes == ["audit:event:write"]`. A second fake returning `scopes: []` yields `scopes.is_empty()`. |
| `pat_resolution_scope_less_token` | Fake returns empty scopes; `verify_pat` resolves the owner with empty scopes (proves R1.3 does not invent scopes). |
| `require_scope_denies_scope_less_pat` | `AuthUser { scopes: [] }` → `require_scope(&user, "audit:event:write")` is `Err(Error::Forbidden(..))`. |
| `require_scope_passes_scoped_pat` | `AuthUser { scopes: ["audit:event:write"] }` → `require_scope(.., "audit:event:write")` is `Ok(())`. |
| `require_scope_denies_unknown_scope_string` | (a) `scopes: ["other:scope"]`, require `"audit:event:write"` → denied; (b) `scopes: ["audit:event:write"]`, require a never-minted string `"unknown:scope"` → denied; (c) `scopes: []`, require `""` / `"   "` → denied. No input combination yields `Ok(())` unless the exact required string is in the carried scopes. |
| `require_scope_denies_jwt_and_bot_principals` | `AuthUser` built the way the JWT path builds it (`scopes: []`) and the way the bot path builds it (`scopes: []`) → `require_scope(.., "audit:event:write")` denied. Pins the invariant that minted audit-scoped PATs are the only grant path. |

**Fail-closed reading**: the five `require_scope` cases assert "deny unless exact match in carried scopes" — there is no assertion anywhere that a missing/unknown scope is granted.

### T-12 — Provisioning gate + end-to-end scope round-trip (PG-gated, `aero-server`)

Test lives in `aero-server/src/pat.rs` `#[cfg(test)]`, gated `#[ignore = "requires live Postgres"]` + `DATABASE_URL` (mirroring `aero-storage/src/pat.rs:190-268`), against a **throwaway migrated database** (fresh `CREATE DATABASE` + `cargo build` then migrate, per AGENTS.md §4.1/4.3; `snaplink_commercial_runtime` seeds `enabled = FALSE` from `0235:14-16`).

Parameterized (claim_validation.rs-style — one table of (relay state, requested scopes, expected outcome)):

| # | Relay state (before test) | Requested scopes | Expected |
|---|---|---|---|
| 1 | `UPDATE snaplink_commercial_runtime SET enabled = FALSE` (default) | `[]` | 201 minted; no gate consulted |
| 2 | `enabled = FALSE` | `["audit:event:write"]` | **403 Forbidden**; `COUNT(*) FROM pat_tokens` unchanged (no row written) |
| 3 | `enabled = FALSE` | `["audit:event:write", "read"]` | **403** (gate fires when the scope is present anywhere in the set) |
| 4 | `enabled = TRUE` + one enabled `snaplink_commercial_bindings` row (per `0235:18-27` schema: `workspace_id/tenant_id/client_id/source_system/revision/enabled`) | `["audit:event:write"]` | 201 minted |

Case 4 round-trip assertions (the direction's "scope round-trips through PatVerifier → AuthUser"):
1. `PatRepo::verify(hash_pat(&token))` returns `(owner, ["audit:event:write"])`.
2. `AuthService::verify_pat(&token)` (service wired with `PatRepo` via `with_pat_verifier`, as in boot) returns the same owner + scopes.
3. Extractor path: `AuthUser` built from the PAT bearer has `participant_id == owner` and `scopes == ["audit:event:write"]`; `require_scope(&user, "audit:event:write")` is `Ok(())`.
4. Same PAT on a scope-less token (case 1 minted) → `AuthUser.scopes` empty → `require_scope` denies.

Implementation note: extract `mint_pat`'s decision core into a testable helper (or drive the route handler directly with `AppState`/`AuthUser` construction) so the table above runs without HTTP plumbing; the HTTP status assertion maps `Error::Forbidden` → 403 via the existing `ApiResult` mapping.

### T-13 — No regression (existing pinned behavior unchanged)

| Test | Pass predicate |
|---|---|
| Extractor fallback order | `extractor.rs:118-139` still tries JWT → `verify_pat` → `verify_bot_token` in that order; for a scope-less PAT, the resolved `AuthUser.participant_id` is **identical** to the pre-change value (same owner; new `scopes` field is empty). Existing `well_formed_pat_*` / `pat_and_bot_token_gates_are_disjoint` / `rejection_renders_as_401_json` / `jwt_helper_*` tests (`service.rs:627-740`, `extractor.rs:150-200`) pass unmodified. |
| Bot tokens scope-less | `verify_bot_token` (`service.rs:460-472`) unchanged; `AuthUser` from the bot path has empty scopes; `BotTokenVerifier` trait (`bot.rs:40`) untouched. |
| JWT path | `auth_user_from_access_claims` still resolves `session_id`/`exp` identically; JWT `AuthUser` scopes empty (zero-alloc shared empty). |
| Storage | `pat_create_verify_revoke_cycle` / `pat_expired_token_does_not_verify` (`aero-storage/src/pat.rs:300-360`) pass with the new `verify` return shape; expired/revoked/unknown tokens still resolve `None` (never a resolution with fabricated scopes). |
| Gate only fires for `audit:event:write` | Mint of `["read"]` (or any non-audit scope) is unaffected by relay state (case 1 of T-12). |

**Gate for the whole change**: `cargo check --workspace` clean · `cargo test --workspace --lib` green (hermetic) · PG-gated tests green with `-- --ignored` on a throwaway DB · `cargo clippy --workspace --all-targets` adds no warnings · `scripts/{truth-check,file-size-check,web-check}.sh` zero violations (AGENTS.md §4.3).

---

## 4. Explicit non-goals / boundaries

- **Route wiring of `require_scope`**: no inbound audit-ingestion route exists today; the guard is the primitive. Wiring it to a route (and any B5-4 endpoint) is a follow-up.
- **Scope allowlist at mint**: only `audit:event:write` is provision-gated (R3.2); arbitrary/unknown scope strings remain stored-not-enforced.
- **Out of scope from the same analysis**: (a) in-tx `audit_events` writes for auth/admin ops + L1 login-failure aggregation (direction #2); (b) single-sourcing the RFC 9068 claim contract into `aero-common` (direction #3).
- **No new crate dependencies**; `aero-auth` stays on `aero-common` + `aero-storage` (Cargo.toml unchanged). `aero-eng` is **not** imported by `aero-auth` or `aero-server` — R3's predicate SQL is a literal mirror of `Q0_SQL` in the server mint module (single 6-line SELECT; do not lift it into a crate).
- **No change** to PAT mint shape, hashing, listing, revocation, or the `last_used_at` bump; no change to JWT/bot semantics.
- **Migration**: none required (`scopes` column already exists, `0019_pat.sql:23`).
