# Design — Carry PAT scopes through verification + fail-closed `require_scope` guard

- **Module**: `crates/aero-auth` (+ `crates/aero-storage/src/pat.rs`, `crates/aero-server/src/{pat,ip_allowlist,identity_migrations}.rs`)
- **Source spec**: `docs/requirements/2026-08-08-pat-scope-carry-require-guard.md`
- **Date**: 2026-08-08
- **Status**: Design — every evidence citation independently re-verified against the repo; the verification ledger below is the acceptance basis. Security-review deltas B1–B5, Flag 2 (who-vs-when decision), and the `require_scope` module-doc discipline are applied in §3.4/§3.5/§4/§5/§6/§7/§8 with live-source anchors.

---

## 1. Evidence verification ledger (untrusted claims → verdicts)

All citations in the spec's §1.1 table were re-checked against live source. **Every claim confirmed**, with three acceptance-relevant omissions and one factual correction discovered:

| # | Claim | Verdict | Anchor (verified) |
|---|---|---|---|
| 1 | `PatVerifier::verify` drops scopes; only implementor is `PatRepo` | ✅ | Trait `async fn verify(&self, token_hash: &str) -> Option<ParticipantId>` at `crates/aero-auth/src/pat.rs:30`; blanket impl `:38-52` (DB err → `None` + `tracing::warn!`). `rg "impl.*PatVerifier"` → only the blanket impl. |
| 2 | `AuthUser` is `{participant_id, session_id, exp}`, `#[derive(Debug, Clone, Copy)]` | ✅ | `extractor.rs:33-40`; `Copy` reliance at `extractor.rs:95` (`parts.extensions.get::<AuthUser>().copied()`). Not `Serialize` — no wire-format change. |
| 3 | `verify_pat` = `is_well_formed_pat` gate → `hash_pat` → verifier | ✅ | `service.rs:434-447`; `pat_verifier: Option<SharedPatVerifier>` at `service.rs:66`. |
| 4 | `PatRepo::verify` reads only `participant_id`; scopes persisted; no NULL seam | ✅ | SELECT at `storage/pat.rs:139`; INSERT binds `scopes` at `:114`; `PatSummary.scopes` at `:37`; `scopes TEXT[] NOT NULL DEFAULT '{}'` at `migrations/0019_pat.sql:23` (exact line). |
| 5 | OIDC client-credentials scope precedent | ✅ | `required_scopes` `oidc.rs:378-381`; `granted_scopes()` BTreeSet union `:401-408`; enforced at `:478` (any-missing → error). |
| 6 | `SCOPE_AUDIT = "audit:event:write"`, outbound only | ✅ | `client.rs:43` (exact line); used at `client.rs:426` (token acquisition) + `:289-292` (response claim validation) — both outbound legs of the connector. |
| 7 | Q0 relay-health predicate | ✅ | `Q0_SQL` at `audit_provision.rs:34` (exact text); consumed `:536`. `const Q0_SQL` is private, but `audit_provision` is already `pub mod` in `aero-eng/lib.rs` → pub-ifying the const is a one-word change; the literal mirror is **not** the only option (resolved in G-D). |
| 8 | Runtime seeds `FALSE`; bindings schema | ✅ | `0235:10` `enabled BOOLEAN NOT NULL DEFAULT FALSE`; `:14-16` singleton row `VALUES (TRUE, FALSE)`; bindings `:22-27` (`workspace_id PK, tenant_id/client_id/source_system UNIQUE, revision, enabled`). |
| 9 | No `require_scope` anywhere | ✅ | `rg require_scope crates/aero-auth/src crates/aero-server/src` → exit 1, zero hits. |
| 10 | Bot tokens scope-less by design | ✅ | `bot.rs:40` `BotTokenVerifier::verify(&self, token: &str) -> Option<ParticipantId>`. |
| 11 | 403-equivalent primitive | ✅ | `Error::Forbidden(String)` `error.rs:19` → HTTP 403 `:70`, code `"forbidden"` `:85`. |
| 12 | Mint route shape | ✅ | `server/pat.rs`: `MAX_SCOPES=64` `:52`; "Reserved for future fine-grained authorization; stored, not yet enforced" `:69-72`; `normalize_scopes` `:102-115`; `mint_pat` `:120-152`; `pat_repo(&s)` helper via `s.participants.pool()`. `AppState.pg` exists (`state.rs:383`). |
| 13 | Hermetic test infra | ✅ | `token_service()` with `connect_lazy` `service.rs:605-615`; `well_formed_pat_*` `:630-676`; `pat_and_bot_token_gates_are_disjoint` `:700+`. |
| 14 | PG-gated db_tests | ✅ | `storage/pat.rs:278-360` (`pool()` + `pat_create_verify_revoke_cycle` asserting `Some(owner)` + `pat_expired_token_does_not_verify`). `#[ignore]` PG tests already exist in `aero-server` (`message_sentiment.rs`, `message_send_policy.rs`, `saved_search_monitor.rs`). |
| 15 | No inbound audit-ingest route | ✅ | `rg -i audit routes/routes.rs` → no matches. Route wiring of the guard is genuinely out of scope. |
| 16 | `lib.rs:33` re-export; `SharedPatVerifier` `pat.rs:56` | ✅ | Exact. |
| 17 | Extractor fallback order JWT → PAT → bot; line refs 118-139/127-133/130-132/143 | ✅ | Exact per line-numbered read. |

### 1.1 Gaps found in the spec's acceptance (must be folded into the design)

**G-A (spec misses a `verify_pat` caller).** `crates/aero-server/src/ip_allowlist.rs:174` calls `s.auth.verify_pat(token)` and maps the resolution to `AuthUser { participant_id, session_id: None, exp: None }`. R1.3's return-type change breaks this call site, and the built `AuthUser` needs the new `scopes` field. Same for `verify_bot_token` at `:185`. The spec's T-13 never mentions `ip_allowlist.rs`.

**G-B (spec misses direct `PatRepo::verify` assertions).** `crates/aero-storage/src/participant/db_tests.rs:515,524,549` assert `pats.verify(&hash) == Some(id)` / `None`. The storage return-shape change forces mechanical updates here, not just in `storage/pat.rs` db_tests as T-13 claims.

**G-C (spec's "existing tests pass unmodified" is too strong).** `crates/aero-server/src/identity_migrations.rs:306,319` construct `AuthUser { … }` literally inside `#[cfg(test)]`. A required `scopes` field breaks these two tests (mechanical one-line addition).

**G-D (factual correction → decision change).** The spec claims "`aero-eng` is not imported by `aero-auth` or `aero-server`". False for `aero-server`: `aero-server/Cargo.toml:30` already has `aero-eng.workspace = true` (also `aero-audit-connector.workspace = true` at `:36`). Because the dep exists and is unconditional (the `aero-cli` bin shares the crate), the mint gate **imports `Q0_SQL` as the single source** instead of mirroring it: `Q0_SQL` becomes `pub` in `aero-eng/src/audit_provision.rs:34` (one-word change; the module is already `pub mod`; no new dep, no feature, no cycle — aero-eng deps aero-common only). A security gate whose predicate silently diverges from the canonical relay-health query is a latent enforcement gap; the import makes divergence impossible by construction. The gate also **imports `aero_audit_connector::client::SCOPE_AUDIT`** (dep exists, `pub mod client`), avoiding a third copy of the string (two copies exist today: `client.rs:43` and a local `const SCOPE_AUDIT` in `server/src/snaplink_commercial/http.rs:23`). Unifying those copies is a non-goal.

### 1.2 Observability infra verification (F8 deltas — §3.7)

The security review's two observability deltas (mint-gate counter **now**; wire-time convention **pinned now**) were verified against live `/metrics` + telemetry infra:

| Infra piece | Verified anchor | Consequence for §3.7 |
|---|---|---|
| `/metrics` exposure | `routes.rs:547` mounts `metrics::metrics_handler`; bearer-gated by `AERO_METRICS_TOKEN` (`metrics.rs:308-318`); renders the global registry (`render_prometheus`, `metrics.rs:866`) | New counters appear on `/metrics` with **zero extra wiring** — emit is enough |
| Counter API | `inc_counter_labeled` auto-creates on first use, never panics; kind conflicts ignored (`metrics.rs:499-519`) | Emit-only is safe; `register_help` optional/idempotent (`runtime.rs:316` precedent) |
| Labeled-counter house style | `aero_ai_moderation_skipped_total{reason=…}` (`moderation_bot.rs:64,205,261`), `aero_av_scan_total{result=…}` (`av_scan.rs:168,306`), `aero_messages_sent_total{room_type=…}` (`aero-im-core/…/messages.rs:249`), `aero_webhook_delivery_outcomes_total` (`webhooks/runtime.rs:294`) | `{scope,reason}` two-label counter, `_total` suffix, `aero_` prefix = house convention |
| Server-local name precedent | `RATE_LIMIT_REJECTIONS_TOTAL` lives in `aero-server/src/metrics.rs:54` (no `register_help`; auto-created on first inc), tested via `render_prometheus().contains(...)` (`:611-615`) | `PAT_MINT_SCOPE_DENIED_TOTAL` (and the future `AUTHZ_SCOPE_DENIED_TOTAL`) live there, same test style |
| request-id correlation | `inject_request_id` (`routes.rs:51-84`): `RequestId` extension + `info_span!("http_request", request_id = …)`; the fmt subscriber renders active span fields — every structured line emitted while serving a request is tagged | Any `tracing::warn!` inside a handler (mint gate included) is auto-tagged `request_id`; the convention must **not** re-log the id |
| Principal-kind derivation | `AuthUser { session_id, exp, scopes }`: JWT path sets both `Some` (`extractor.rs:52`); PAT/bot set both `None` (`:127-134`); scopes empty for JWT/bot, non-empty ⇒ PAT | `session_id`/`exp` presence → `jwt`; `!scopes.is_empty()` → `pat`; else `bot` (§3.7.2) |
| 403 mapping | `Error::Forbidden` → HTTP 403, `code:"forbidden"` (`error.rs:19,:70,:85`, ledger §1 #11) | Denial counter+log accompany the actionable 403 body, unchanged |

---

## 2. Design overview

Three layers, each independently testable:

1. **R1 — carry**: `PatRepo::verify` returns owner+scopes; `PatVerifier::verify` and `AuthService::verify_pat` return `PatResolution`; `AuthUser` gains `scopes: Arc<[String]>` (keeps `Copy`).
2. **R2 — enforce**: pure `require_scope(user, scope)` fail-closed guard, hermetic, no DB at guard time.
3. **R3 — mint gate**: `mint_pat` rejects `audit:event:write` unless the Q0 relay-health predicate holds; DB error → fail closed.

```mermaid
flowchart LR
  M["mint_pat (aero-server)"] -->|normalize_scopes| G["audit-scope provision gate (imports Q0_SQL)"]
  G -->|unhealthy| F["403 Forbidden — no row written"]
  G -->|healthy| C["PatRepo::create — scopes TEXT[]"]
  C --> V["PatRepo::verify — SELECT participant_id, scopes"]
  V --> R["PatResolution {participant_id, scopes}"]
  R --> AU["AuthUser.scopes: Arc[String]"]
  AU --> Q["require_scope — exact match, else 403"]
```

---

## 3. API changes

### 3.1 `crates/aero-storage/src/pat.rs`

```rust
// verify: return shape change (the only change to this fn's contract)
pub async fn verify(
    &self,
    token_hash: &str,
) -> Result<Option<(ParticipantId, Vec<String>)>, sqlx::Error>
```

- SQL becomes `SELECT participant_id, scopes FROM pat_tokens WHERE …` (active-token predicate, deleted-participant EXISTS guard, best-effort `last_used_at` bump — all unchanged). `query_as::<_, (uuid::Uuid, Vec<String>)>` — sqlx decodes `TEXT[]` (`'{}'` → empty `Vec`); `NOT NULL` means no `COALESCE` seam (spec R1.2, confirmed `0019_pat.sql:23`).
- Tuple chosen over a named struct: smallest diff to existing assertions; the domain shape `PatResolution` lives in `aero-auth` (storage cannot reference it — `aero-auth` depends on `aero-storage`, not vice versa).

### 3.2 `crates/aero-auth/src/pat.rs`

```rust
/// Resolution of a verified PAT: owner + the scopes persisted at mint.
/// Unknown/revoked/expired → `None` (unchanged); DB error → `None` + warn
/// (unchanged) — a failed lookup never fabricates scopes.
#[derive(Debug, Clone)]
pub struct PatResolution {
    pub participant_id: ParticipantId,
    pub scopes: Vec<String>,
}

#[async_trait]
pub trait PatVerifier: Send + Sync {
    async fn verify(&self, token_hash: &str) -> Option<PatResolution>;
}

#[async_trait]
impl PatVerifier for aero_storage::PatRepo {
    async fn verify(&self, token_hash: &str) -> Option<PatResolution> {
        match aero_storage::PatRepo::verify(self, token_hash).await {
            Ok(Some((participant_id, scopes))) => Some(PatResolution { participant_id, scopes }),
            Ok(None) => None,
            Err(err) => { tracing::warn!(error = %err, "PAT verification query failed"); None }
        }
    }
}
```

`SharedPatVerifier = Arc<dyn PatVerifier>` unchanged. `PatResolution` is `Send + Sync` (plain data) satisfying the trait-object bound.

### 3.3 `crates/aero-auth/src/service.rs`

```rust
pub async fn verify_pat(&self, token: &str) -> Option<PatResolution> {
    let verifier = self.pat_verifier.as_ref()?;
    if !is_well_formed_pat(token) { return None; }
    let hash = aero_storage::pat::hash_pat(token);
    verifier.verify(&hash).await
}
```

Structural gate and not-wired → `None` unchanged. `verify_bot_token` untouched (`:460-472`).

### 3.4 `crates/aero-auth/src/extractor.rs`

```rust
#[derive(Debug, Clone, Copy)]
pub struct AuthUser {
    pub participant_id: ParticipantId,
    pub session_id: Option<SessionId>,
    pub exp: Option<u64>,
    /// Scopes carried by the presented credential. Empty for JWTs and bot
    /// tokens (both scope-less by design); a PAT carries the scopes persisted
    /// at mint. Read-only snapshot taken at verification time.
    pub scopes: Arc<[String]>,
}

/// One shared empty instance — JWT/bot paths never allocate per request.
static EMPTY_SCOPES: std::sync::OnceLock<Arc<[String]>> = std::sync::OnceLock::new();
fn empty_scopes() -> Arc<[String]> {
    EMPTY_SCOPES.get_or_init(|| Arc::from(Vec::<String>::new())).clone()
}
```

- `auth_user_from_access_claims` (JWT, `:52`): `scopes: empty_scopes()`.
- PAT branch (`:127-133`): `verify_pat(token).await` → `Some(res)` → `AuthUser { participant_id: res.participant_id, session_id: None, exp: None, scopes: Arc::from(res.scopes) }`; `None` → falls through to bot as today.
- Bot branch (`:130-132`): `scopes: empty_scopes()`.
- **Extension stash handoff (middleware → extractor — direction matters)**: the *only* pre-stasher of `AuthUser` is `enforce_layer` (`ip_allowlist.rs:60`), which resolves identity in `authenticated_user` (`:155`) and inserts the typed user at `:108` before the handler's extractor runs; the extractor reuses it at `:95` (early return `:96`, no second decode). The earlier draft's "insert `AuthUser` in the extractor for downstream layers" idea is **dropped**: the extractor runs *after* the middleware, so nothing it inserts can reach `enforce_layer`, and on the stashed path it returns before its own `parts.extensions.insert(user.participant_id)` (`:143`) — harmless, since no layer reads `ParticipantId` from extensions (verified: the only `extensions.get::<…>` in the tree is `extractor.rs:95`).
- **B2 invariant (comment + pins, lands in step 6)**: next to the existing trusted-edge comment at `extractor.rs:91-94`, pin *"only `enforce_layer` may pre-stash `AuthUser`, and the stashed value must carry verification-time scopes"* — a future middleware stashing from a different source (cookie, downstream header) would become an identity-confusion vector at the `:95` read. Three-fold pin: doc comment here, hermetic T-11 test `extractor_serves_prestashed_authuser_verbatim` (panic-state proof that the handoff never derefs the service), static rg pin in T-13 (the only `AuthUser`-valued insert sites are `ip_allowlist.rs:108` and the extractor's post-verification stash).
- **B1 invariant (pin, lands in step 9; round-trip in T-12 assertion 5)**: `authenticated_user`'s PAT branch (`ip_allowlist.rs:174-177`) currently maps only `participant_id` (`Some(owner) => AuthUser { participant_id, … }`); post-change it must carry scopes: `Some(res) => AuthUser { participant_id: res.participant_id, session_id: None, exp: None, scopes: Arc::from(res.scopes) }`. A mechanical slip (map only `participant_id`) stashes a scope-less `AuthUser` that the extractor serves on **every** PAT-authenticated `/api/*` request — fail-closed but feature-broken (silent 403s on audit routes). Bot branch (`:185-188`) and JWT branch (`:193`) keep `empty_scopes()`.
- `Arc<[String]>` keeps `Copy` (verified: only `Copy` reliance is `:95` `.copied()`); `Vec<String>` would force `.cloned()` at `:95` and drop `Copy` — rejected.

### 3.5 `crates/aero-auth/src/scope.rs` (new)

**Module-doc discipline (security pin)**: callers pass **compile-time constants only** — `aero_audit_connector::client::SCOPE_AUDIT` — never user-controlled text: the denial message echoes the required-scope argument verbatim (`format!("missing required scope: {scope}")`), so a user-controlled argument would be reflected back (message-leakage / log-injection surface). The future wired route must not template a dynamic scope into the guard.

```rust
/// Fail-closed scope guard. Grants only when the exact required scope string
/// is present in the scopes the credential carried at verification time.
/// Pure — no DB read at guard time (scopes are immutable after mint; the
/// verification-time snapshot is the single source of truth, and the guard
/// stays unit-testable without a pool).
///
/// **Caller discipline (security pin): pass compile-time constants only**
/// (e.g. `aero_audit_connector::client::SCOPE_AUDIT`) — never user-controlled
/// text: the denial message echoes the required-scope argument verbatim, so a
/// user-controlled argument would be reflected back (message-leakage / log-
/// injection surface). The guard itself stays log-free (F8).
pub fn require_scope(user: &AuthUser, scope: &str) -> aero_common::Result<()> {
    if scope.is_empty() || scope.trim().is_empty() {
        return Err(aero_common::Error::Forbidden(
            "empty required scope".into(),
        ));
    }
    if user.scopes.iter().any(|carried| carried == scope) {
        Ok(())
    } else {
        Err(aero_common::Error::Forbidden(format!(
            "missing required scope: {scope}"
        )))
    }
}
```

Re-export: `crates/aero-auth/src/lib.rs` — `pub use pat::{PatResolution, PatVerifier, SharedPatVerifier};` and `pub use scope::require_scope;` (no root re-export collisions — no other `require_scope` exists, verified §1 #9).

### 3.6 `crates/aero-server/src/pat.rs` (mint gate, R3)

```rust
use aero_audit_connector::client::SCOPE_AUDIT; // dep exists (Cargo.toml:36); single source
use aero_eng::audit_provision::Q0_SQL; // pub-ified (G-D); single source — do NOT re-mirror

/// R3 gate: reject audit-scoped mint unless the relay is provisioned.
/// Fail-closed: DB error → Err (no token minted). Pure decision, testable
/// with a live pool (T-12) without HTTP plumbing.
async fn audit_scope_provision_gate(pool: &sqlx::PgPool, scopes: &[String]) -> AeroResult<()> {
    if !scopes.iter().any(|s| s == SCOPE_AUDIT) {
        return Ok(()); // all non-audit scopes keep stored-not-enforced behavior
    }
    let row = sqlx::query_as::<_, (bool, i64)>(Q0_SQL)
        .fetch_optional(pool)
        .await
        .map_err(|e| {
            record_mint_scope_denial("db_error", scopes); // F2: fail closed, 500 — count BEFORE the Err
            AeroError::from(e)
        })?; // DB error (incl. missing 0235 tables) → fail closed, 500
    match row {
        Some((true, n)) if n > 0 => Ok(()),
        _ => {
            record_mint_scope_denial("relay_unhealthy", scopes); // F3: 403, no pat_tokens row
            Err(AeroError::Forbidden(
                "audit:event:write requires the audit relay to be provisioned".into(),
            ))
        }
    }
}

/// Warn + counter for a fail-closed mint rejection (§3.7.1). Runs inside the
/// `http_request` span (`inject_request_id`, `routes.rs:62-73`), so every line
/// carries `request_id` automatically. `reason` is a compile-time constant from
/// the two call sites; `requested_scopes` is the normalized set (≤64 strings) —
/// never the token, never the `Authorization` header (echo discipline, §3.5).
fn record_mint_scope_denial(reason: &'static str, requested_scopes: &[String]) {
    tracing::warn!(
        scope = SCOPE_AUDIT,
        reason,
        requested_scopes = ?requested_scopes,
        "PAT mint denied: audit:event:write requires a provisioned audit relay"
    );
    aero_common::metrics::inc_counter_labeled(
        crate::metrics::PAT_MINT_SCOPE_DENIED_TOTAL,
        1,
        &[("scope", SCOPE_AUDIT), ("reason", reason)],
    );
}
```

In `mint_pat` (`:120-152`), after `normalize_scopes` and **before** `pat_repo(&s).create(...)`:

```rust
audit_scope_provision_gate(&s.pg, &scopes).await?; // 403 / 500, no pat_tokens row written
```

`AppState.pg` exists (`state.rs:383`); no new state field.

### 3.7 Observability: mint-gate counter (lands now) + wire-time denial convention (pinned now)

> Security-review delta: the "silent" part of silent-403 must be bounded on both sides —
> mint-gate denial volume as a Prometheus counter **now**, and a single wire-time
> convention for the future route **defined now** so no second convention is invented.
> Infra verified against `/metrics` + telemetry in §1.2.

#### 3.7.1 Mint-gate counter — `aero_pat_mint_scope_denied_total` (this change)

Name constant in `aero-server/src/metrics.rs` (server-local family, beside `RATE_LIMIT_REJECTIONS_TOTAL` `:54`; cross-crate names live in `aero_common::metrics::names`):

```rust
/// Counter (server-local name): fail-closed PAT mint rejections by the R3
/// audit-scope provision gate, labeled by `scope` (the compile-time
/// `SCOPE_AUDIT` const) and `reason` = `relay_unhealthy` | `db_error`. Denial
/// volume is the probe signal for misconfigured clients (minting without a
/// provisioned relay). Emitted only from `audit_scope_provision_gate`.
pub const PAT_MINT_SCOPE_DENIED_TOTAL: &str = "aero_pat_mint_scope_denied_total";
```

Emit at **both** denial branches of the gate (DB error **and** unhealthy row — §3.6 code), so a regression that silently flips the gate to allow-all shows as a counter that stops growing while mints succeed. `requested_scopes` is the normalized set (≤64 strings); `reason` is a compile-time constant from the two call sites.

- **Cardinality bounded**: `scope` is the compile-time `SCOPE_AUDIT` const, `reason` is a fixed two-value set — the series count is 2, period.
- **No `register_help` required**: `inc_counter_labeled` creates the series on first use (`metrics.rs:499-519`) — same as `RATE_LIMIT_REJECTIONS_TOTAL` (no registration); `register_help` is idempotent if help text is wanted later (precedent `runtime.rs:316`).
- **Relationship to the observability-design gauges** (`aero_audit_relay_scope_provisioned` & friends, `docs/design/2026-08-08-aero-auth-b5-4-scope-provisioning-observability.design.md` §2.2): gauges = gate *state*, this counter = denial *volume*; complementary, no overlap.

#### 3.7.2 Wire-time denial convention — `aero_authz_scope_denied_total` + warn (future route, pinned now)

Applies to **every** future surface that maps `require_scope`'s `Err` to a 403. **No code lands now** — a standalone helper would be a zero-call builder (truth-check red; §8) — but the convention is pinned here so the route *wires* it instead of *inventing* it:

| Element | Convention (fixed) |
|---|---|
| Metric | `aero_authz_scope_denied_total{scope="<required-scope-const>"}` — counter, +1 per denied request at the route boundary. `scope` = the compile-time required-scope constant (e.g. `SCOPE_AUDIT`), never caller-supplied text. **No principal label** — cardinality stays = scope-set size; principal kind goes in the log. Constant added to `aero-server/src/metrics.rs` when the route lands. |
| Log place | `tracing::warn!` at the route boundary (the handler that calls `require_scope`), **not** inside the guard — the guard stays pure and log-free (F8). |
| `request_id` | Automatic: the handler runs inside the `http_request` span created by `inject_request_id` (`routes.rs:62-73`) and the fmt subscriber renders active span fields — **never** re-log the id as a field (the span carries it; duplicating invites drift). |
| Log fields (mandatory) | `scope` (required-scope const), `principal` = kind derived from the `AuthUser` (below), `participant_id` (internal id; audit precedents log it — optional but must be a field, never interpolated). |
| Log fields (forbidden) | The token, the `Authorization` header, the PAT hash — never. |
| Fixed message (grep anchor) | `"authorization denied: missing required scope"` — scope/principal are fields, the message is constant so `rg` finds every denial site. |
| 403 body | Unchanged from `require_scope` (`"missing required scope: {scope}"` → 403 `code:"forbidden"`, `error.rs:19,:70,:85`); the body is the client signal, the log+metric are the ops signal. |

Principal-kind derivation (snippet lands with the route; pinned here so it is not re-derived): JWT sets `session_id`+`exp` (`extractor.rs:52`), PAT/bot set both `None` (`:127-134`), scopes empty for JWT/bot by construction — so:

```rust
fn principal_kind(user: &AuthUser) -> &'static str {
    if user.session_id.is_some() || user.exp.is_some() { "jwt" }
    else if !user.scopes.is_empty() { "pat" }
    else { "bot" }
}
```

Surface rule carried from the security review (it feeds the convention): human audit READ surfaces gate by role (`effective_member_role`/`can_administer`), **not** by `require_scope` (the §8 Flag-2 decision); a wrongly scope-gated human surface surfaces immediately as `principal="jwt"` denial rows — the log is the discoverability mechanism.

---

## 4. Compatibility constraints

| Constraint | Consequence |
|---|---|
| `AuthUser` new required pub field | Compiler-enforced: all 7 literal construction sites must add `scopes` — `extractor.rs:52,134`, `ip_allowlist.rs:176,187,194`, `identity_migrations.rs:306,319`. No silent runtime break. |
| `AuthUser` must stay `Copy` | `Arc<[String]>` (spec R1.4); `Vec<String>` rejected. |
| `AuthUser` has no serde derives | No wire/JSON format change; `PatSummary` listing already carries scopes — untouched. |
| `PatVerifier` trait return change | Zero external implementors (blanket impl over `PatRepo` only); only `aero-auth` internals + `ip_allowlist.rs:174` consume `verify_pat`. |
| `PatRepo::verify` return change | Consumers: blanket impl (`auth/pat.rs`), `storage/pat.rs` db_tests `:300-360`, `storage/participant/db_tests.rs:515,524,549` (G-B). All compiler-enforced. |
| No DB migration | `scopes` column exists since `0019` with `NOT NULL DEFAULT '{}'`; no `COALESCE`; no NULL seam (spec R1.2). |
| No new crate deps | `aero-auth` unchanged (`aero-common` + `aero-storage`); `aero-server` uses existing `aero-audit-connector` dep for `SCOPE_AUDIT` and its existing `aero-eng` dep for `Q0_SQL` (both deps already in `aero-server/Cargo.toml`), plus its existing `sqlx`/`AppState.pg`. One-word visibility change in `aero-eng` (`const` → `pub const Q0_SQL`). |
| New server-local metric constant | `PAT_MINT_SCOPE_DENIED_TOTAL` added to `aero-server/src/metrics.rs` beside `RATE_LIMIT_REJECTIONS_TOTAL` (`:54`) — auto-created on first `inc_counter_labeled`, no `register_help` required (`metrics.rs:499-519`); zero name collisions today (`rg pat_mint_scope_denied` → zero hits). The future `AUTHZ_SCOPE_DENIED_TOTAL` follows the same precedent when the route lands (§3.7.2). |
| Scope semantics at mint | `normalize_scopes` (trim blanks, drop empties, ≤64 count bound — no dedup; `server/pat.rs:102-115`) plus a **new per-scope length cap `MAX_SCOPE_LEN = 128` chars** (currently unbounded beyond the body limit — B3); gate fires on **any** occurrence of `audit:event:write` in the set (T-12 case 3). **B3 hard rule**: `normalize_scopes` has no allowlist — it stores any scope string — so **every newly enforced scope must be added to the mint gate (or a mint allowlist introduced) in the same change that wires its route** (see §8). |
| Scopes immutable after mint | No update path exists in `PatRepo` (`create`/`verify`/`list`/`revoke` only — verified). Guard-time snapshot cannot go stale via scope mutation; only revocation removes the grant, and revocation already makes `verify` return `None`. |

---

## 5. Failure modes and mitigations

| # | Failure mode | Behavior | Mitigation / note |
|---|---|---|---|
| F1 | DB error during `PatRepo::verify` | `None` + `tracing::warn!` → 401 (unchanged) | A transient blip must not authenticate anyone; scopes never fabricated. |
| F2 | DB error during mint gate (incl. `0235` tables absent in an un-migrated DB) | `AeroError::Database` → 500, **no token minted** | Fail-closed per spec R3.1; observable via 500 + `warn!` + `aero_pat_mint_scope_denied_total{scope="audit:event:write",reason="db_error"}` (§3.7.1). **Pinned by T-12 case 5** (`DROP TABLE snaplink_commercial_runtime` → gate error → no `pat_tokens` row). |
| F3 | Runtime row missing (`WHERE runtime.singleton` → no row) | `fetch_optional` → `None` → 403 | Spec R3.1 explicit; observable via 403 + `warn!` + counter `reason="relay_unhealthy"` (§3.7.1). **Pinned by T-12 case 4c** (`DELETE FROM snaplink_commercial_runtime` → `None` → 403). |
| F4 | Relay switched off **after** mint | Token keeps carrying `audit:event:write`; `require_scope` passes at request time | **Residual risk (documented, out of scope)**: the gate is mint-time; ongoing Q0 re-check at use time would need a request-path DB read, contradicting R2.1's pure-guard design. Operational answer: revoke audit PATs when de-provisioning (revocation is instant via existing `revoke`). Enumeration: `list_pat` is owner-scoped (`server/pat.rs:154`) — no admin surface exists, so use the §8 B4 query (`'audit:event:write' = ANY(scopes)`) to find the tokens, then revoke. |
| F5 | Blank/whitespace/unknown scope argument to `require_scope` | Denied | Spec R2.1: deny, never "no requirement". |
| F6 | PAT presented with scopes `["other:scope"]` to an audit-gated path | 403 | Exact-match semantics; no prefix/alias matching. |
| F7 | Concurrent mints racing the gate | Both pass (gate is a read); token validity identical | No TOCTOU on mint: the gate's answer is a point-in-time health read, and B5-4's requirement is "grant only after the relay works" — satisfied by the read. |
| F8 | `require_scope` denial is silent in logs | Guard returns `Err`; caller decides logging | **Closed by §3.7** — the guard stays pure and log-free; the **future wired route** must emit `warn!` + `aero_authz_scope_denied_total{scope}` per the pinned wire-time convention (§3.7.2: `request_id` via the `http_request` span, `principal` kind derived from `session_id`/`exp`/`scopes`, scope — never the token). Mint-gate rejection (F2/F3) is logged at **warn** + counted **now** (`aero_pat_mint_scope_denied_total{scope,reason}`, §3.7.1) — a silent zero-claim is the residual risk this closes. |
| F9 | T-12 case 2/3 regression (gate not firing) | Test fails | Case 3 pins the "present anywhere in the set" semantics (any-match, not all-match — matches OIDC's any-missing → error inverse). |

---

## 6. Migration steps (compile-ordered; no DB migration)

> Per AGENTS.md §4.2: no `migrations/` change, so no build-before-migrate dance; the gate is `cargo build` + `cargo check --workspace` driven.

1. **`aero-storage/src/pat.rs`** — `verify` SELECT adds `scopes`; return `Result<Option<(ParticipantId, Vec<String>)>, sqlx::Error>`; doc update.
2. **`aero-storage/src/pat.rs` db_tests** — `pat_create_verify_revoke_cycle` `:310` asserts scopes round-trip (`Some((owner, vec!["read","write"]))`); add scope-less mint assertion; `pat_expired_token_does_not_verify` `None` unchanged.
3. **`aero-storage/src/participant/db_tests.rs`** (G-B) — `:515` → `Some((id, vec![]))`; `:524,549` stay `None` (no scope fabrication on tombstone/revoke — that's the pin).
4. **`aero-auth/src/pat.rs`** — add `PatResolution`; trait + blanket impl to `Option<PatResolution>`.
5. **`aero-auth/src/service.rs`** — `verify_pat` → `Option<PatResolution>`; doc.
6. **`aero-auth/src/extractor.rs`** — `AuthUser.scopes: Arc<[String]>` + `EMPTY_SCOPES` OnceLock; three construction paths; **B2 doc-comment pin next to the trusted-edge note at `:91-94`**: *only `enforce_layer` may pre-stash `AuthUser`, and the stashed value must carry verification-time scopes*. (No new insert in the extractor — the stash handoff is middleware→extractor, §3.4.)
7. **`aero-auth/src/scope.rs`** (new) — `require_scope` + hermetic tests (T-11).
8. **`aero-auth/src/lib.rs`** — re-export `PatResolution`, `require_scope`.
9. **`aero-server/src/ip_allowlist.rs`** (G-A + B1 seam + B1 round-trip) — adapt `:174` to resolution shape, carry `scopes: Arc::from(res.scopes)`; bot path `:185` `empty_scopes()` equivalent (local `std::sync::Arc::from(Vec::<String>::new())` or shared const — the server can define its own empty once, or reuse `AuthUser`'s pattern; simplest: `Arc::from(Vec::new())` inline, two call sites). **Testability seam**: make `authenticated_user` `pub(crate)` and change its first parameter from `&AppState` to `&AuthService` — verified against source it only ever uses `s.auth` (`verify_pat` / `verify_bot_token` / `verify_access`); the sole call site `enforce_layer` passes `&s.auth`. This is what lets the T-12 B1 round-trip drive the real PAT-branch mapping with `AuthService::new(...).with_pat_verifier(Arc::new(PatRepo::new(pool)))` (same shape as `bin/boot/services.rs:60`) instead of a full `AppState`. Behavior-identical; `enforce_layer`'s stash at `:108` unchanged.
10. **`aero-server/src/identity_migrations.rs`** (G-C) — `:306,319` add `scopes: Arc::from(Vec::new())`.
11. **`aero-eng/src/audit_provision.rs`** — `const Q0_SQL` → `pub const Q0_SQL` (`:34`), doc line noting the aero-server mint gate consumes it as the single source. Then **`aero-server/src/pat.rs`** — import `SCOPE_AUDIT` + `Q0_SQL`; add `audit_scope_provision_gate`; call before `create`; add `record_mint_scope_denial` (§3.7.1: warn + `aero_pat_mint_scope_denied_total{scope,reason}` at **both** denial branches); **`aero-server/src/metrics.rs`** — add the `PAT_MINT_SCOPE_DENIED_TOTAL` constant; **add `MAX_SCOPE_LEN = 128` per-scope cap to `normalize_scopes` + unit test `normalize_scopes_rejects_overlong_scope`** (B3); add T-12 test module (incl. counter assertions). **Pre-deploy (B4)**: before this step ships, run the §8 enumeration query and resolve every pre-existing `audit:event:write` PAT (revoke-or-accept) — those rows never passed the gate.
12. **Gate** — `cargo check --workspace` clean → `cargo test --workspace --lib` green → `cargo clippy --workspace --all-targets` no new warnings → `scripts/{truth-check,file-size-check,web-check}.sh` zero violations (AGENTS.md §4.3).

---

## 7. Testable acceptance mapping

### T-11 — Fail-closed posture (hermetic, `aero-auth`, no DB)

New tests in `scope.rs` + `service.rs` `#[cfg(test)]` (mirroring `well_formed_pat_*`; fake `PatVerifier` keyed by hash of a `generate_pat()`-minted token — both helpers are `pub` in `aero_storage::pat`):

| Test | Pass predicate |
|---|---|
| `pat_resolution_carries_scopes` | Fake returns `PatResolution { participant_id: P, scopes: ["audit:event:write"] }`; `AuthService::verify_pat` returns same owner **and** scopes; second fake with `scopes: []` → empty scopes. |
| `pat_resolution_scope_less_token` | Fake empty-scope resolution → owner resolves, `scopes.is_empty()` (no invented scopes). |
| `require_scope_denies_scope_less_pat` | `AuthUser { scopes: [] }` + `"audit:event:write"` → `Err(Error::Forbidden)`. |
| `require_scope_passes_scoped_pat` | `AuthUser { scopes: ["audit:event:write"] }` + same → `Ok(())`. |
| `require_scope_denies_unknown_scope_string` | (a) `["other:scope"]`/audit → denied; (b) `["audit:event:write"]`/`"unknown:scope"` → denied; (c) `[]`/`""` and `[]`/`"   "` → denied. **No input combination yields `Ok` unless exact string present.** |
| `require_scope_denies_jwt_and_bot_principals` | JWT-path-shaped and bot-path-shaped `AuthUser` (both `scopes: []`) → denied. |
| `extractor_serves_prestashed_authuser_verbatim` (**B2 pin**) | Hermetic `extractor.rs` test: build `axum::http::request::Parts`, stash an `AuthUser` with non-empty scopes (exactly as `enforce_layer` does at `ip_allowlist.rs:108`), run `AuthUser::from_request_parts` against a test-local state type whose `FromRef<AuthService>` impl **panics** — the handoff at `:95` must serve the stashed value (participant_id **and** scopes) verbatim without ever dereffing the service. Pins the trust boundary: the only code that can insert an `AuthUser` extension is `enforce_layer` (the pre-stasher) and the extractor's own post-verification stash; a future third stasher would be served verbatim, which is exactly why the "only `enforce_layer` may pre-stash, carrying verification-time scopes" invariant is load-bearing. |

### T-12 — Provisioning gate + end-to-end round-trip (PG-gated `#[ignore]`, `aero-server/src/pat.rs`)

Pattern: existing `#[ignore]` PG tests in `aero-server` (`message_sentiment.rs` etc.); throwaway migrated DB per AGENTS.md §4.3 (migrations 0019 + 0235 included — the gate's tables exist).

| # | Relay state (Q0 shape) | Requested scopes | Expected |
|---|---|---|---|
| 1 | default (`enabled = FALSE`) — `(false, _)` | `[]` | 201 minted, gate not consulted |
| 1a | default (`enabled = FALSE`) — `(false, _)` | `["read", "profile:view"]` | **201 minted — non-audit scope passthrough pinned**: the gate consults nothing for non-audit scopes; stored-not-enforced behavior is preserved even while the relay is unhealthy |
| 2 | `enabled = FALSE` — `(false, _)` | `["audit:event:write"]` | 403; `COUNT(*) FROM pat_tokens` unchanged |
| 3 | `enabled = FALSE` — `(false, _)` | `["audit:event:write", "read"]` | 403 (any-occurrence semantics) |
| 4 | `enabled = TRUE` + one enabled binding row — `(true, n>0)` | `["audit:event:write"]` | 201 minted |
| 4b | `enabled = TRUE`, **zero enabled bindings** — `(true, 0)` | `["audit:event:write"]` | 403 (second unhealthy shape: Q0's predicate needs `enabled_bindings > 0`) |
| 4c | **singleton row deleted** (`DELETE FROM snaplink_commercial_runtime`) — `None` (F3) | `["audit:event:write"]` | 403 (`fetch_optional` → `None` → fail closed); no row written |
| 5 | **table dropped** (`DROP TABLE snaplink_commercial_runtime`) — DB error (F2) | `["audit:event:write"]` | 500 (`Error::Database` → `error.rs:75`); `COUNT(*) FROM pat_tokens` unchanged — DB-error fail-closed pinned |

Each row asserts its branch independently on the same throwaway migrated DB; restore the runtime/bindings rows (re-insert singleton / re-create table) between rows.

Case 4 round-trip assertions (mint → `PatRepo::verify` → `AuthService::verify_pat` → extractor-shaped `AuthUser` → **middleware stash → extractor handoff** → `require_scope`):
1. `PatRepo::verify(&hash_pat(&token))` == `Some((owner, ["audit:event:write"]))`.
2. `AuthService::verify_pat(&token)` (via `with_pat_verifier(Arc::new(PatRepo::new(pool)))` as in `bin/boot/services.rs:60`) returns same owner+scopes.
3. Extractor-shaped `AuthUser` (PAT branch construction) has `scopes == ["audit:event:write"]`; `require_scope(&user, SCOPE_AUDIT)` is `Ok`.
4. Case-1-minted scope-less token → `AuthUser.scopes` empty → `require_scope` denies.
5. **B1 round-trip (middleware-stash → extractor handoff)** — the gap the security review flagged: assertions 1–4 cover extractor-shaped construction only, never the middleware identity source. Mint an audit-scoped PAT (case 4) → build `Authorization: Bearer aero_pat_…` headers → `authenticated_user(&auth, &headers)` (the real `ip_allowlist.rs:174` PAT branch, `pub(crate)` per §6 step 9) → assert the returned `AuthUser.scopes == ["audit:event:write"]` (a mechanical slip mapping only `participant_id` fails here) → stash exactly as `enforce_layer` does (`parts.extensions.insert(user)`, `ip_allowlist.rs:108`) → `AuthUser::from_request_parts(&mut parts, &auth)` (the `:95` handoff; `AuthService` as state via axum-core's identity `impl<T> FromRef<T> for T`, `from_ref.rs:18`, and the stash short-circuit never derefs it) → assert the served user is identical (same participant_id **and** scopes — the handoff is value-verbatim) → `require_scope(&served, SCOPE_AUDIT)` is `Ok`. Negative leg: a case-1-minted scope-less PAT through the same path → `scopes` empty after the handoff → `require_scope` denies.

Implementation: drive `audit_scope_provision_gate` + `PatRepo` directly (the gate helper is the extracted decision core; no HTTP plumbing needed for the table — assert the gate-level outcomes `Ok` / `Err(Forbidden)` / `Err(Database)`; one HTTP-level test, or the `error.rs:70,75` mapping T-13 already pins, asserts 403/500 rendering).

**Counter assertions (§3.7.1)**: rows 2/3 additionally assert `metrics::render_prometheus().contains("aero_pat_mint_scope_denied_total{scope=\"audit:event:write\",reason=\"relay_unhealthy\"}")` (rows 4b/4c hit the same series; presence-of-labeled-line precedent: `metrics.rs:611-615`, `bot_dispatch/tests.rs:257`, `av_scan.rs:306`); row 5 (`DROP TABLE`) asserts the `reason="db_error"` labeled line. Rows 1/1a's 201 asserts the gate's early return — the counter code is unreachable for non-audit scopes by construction. A separate hermetic test drives the `db_error` branch with `PgPoolOptions::connect_lazy("postgres://nobody:wrong@127.0.0.1:1/none")` (first query fails fast with ECONNREFUSED — no real DB) and asserts the `reason="db_error"` labeled line — proving **both** denial branches count even without PG.

### T-13 — No regression (pins, including the three gaps)

| Pin | Predicate |
|---|---|
| Extractor fallback order | JWT → `verify_pat` → `verify_bot_token` order preserved (`extractor.rs:118-139`); scope-less PAT resolves the **same owner** as pre-change. Existing `well_formed_pat_*`, `pat_and_bot_token_gates_are_disjoint`, `rejection_renders_as_401_json`, `jwt_helper_*` pass unmodified. |
| Bot tokens scope-less | `verify_bot_token` and `BotTokenVerifier` untouched; bot-path `AuthUser.scopes` empty. |
| JWT path | `auth_user_from_access_claims` resolves `session_id`/`exp` identically; scopes empty via shared `OnceLock` instance (assert `Arc::ptr_eq` across two calls — zero-alloc pin). |
| Storage | `pat_create_verify_revoke_cycle` / `pat_expired_token_does_not_verify` pass with new shape; expired/revoked/unknown → `None` (never a fabricated resolution). **Plus G-B**: `participant/db_tests.rs:515,524,549` pass (tombstoned owner → `None`, no scopes). |
| Gate fires only for `audit:event:write` | Case-1 mint of `["read"]` unaffected by relay state. |
| **G-A pin (new, incl. B1)** | `authenticated_user`'s PAT branch (`ip_allowlist.rs:174-177`) builds `AuthUser` carrying **verification-time scopes** (`Arc::from(res.scopes)`) — static assertion plus the T-12 round-trip assertion 5 (the stash→handoff path, which catches a participant_id-only slip even if the branch construction looks right in isolation); scope-less PAT → empty scopes; bot (`:185-188`) / JWT (`:193`) paths unchanged; `enforce_layer` behavior identical after the `&AuthService` signature refactor (§6 step 9). |
| **B2 pin (new)** | Only `enforce_layer` (`ip_allowlist.rs:108`) pre-stashes `AuthUser`; extractor doc comment (`extractor.rs:91-94`) states the invariant; hermetic T-11 test `extractor_serves_prestashed_authuser_verbatim` proves the `:95` read serves a stashed value verbatim without dereffing the service; static rg pin: `AuthUser`-valued extension inserts appear **only** at `ip_allowlist.rs:108` and the extractor's own post-verification stash — no third stasher may exist. |
| **G-C pin (new)** | `identity_migration_*` tests pass with the `scopes` field added (mechanical). |

**Full gate**: `cargo check --workspace` clean · `cargo test --workspace --lib` green · PG-gated `-- --ignored` green on throwaway DB · `cargo clippy --workspace --all-targets` no new warnings · `scripts/{truth-check,file-size-check,web-check}.sh` zero violations.

---

## 8. Non-goals / residual risks (unchanged from spec §4, plus)

- Route wiring of `require_scope`: no inbound audit-ingest route exists (verified §1 #15); the deliverable is the primitive + mint gate + tests.
- **B3 — hard rule (new)**: `normalize_scopes` has no allowlist (`server/pat.rs:102-115` stores any scope string, count-capped only) — `audit:event:write` is the *only* gated scope and every other string is stored-not-enforced. Therefore: **every newly enforced scope must be added to the mint gate (or a mint allowlist introduced) in the same change that wires its route**; a route enforcing a scope with no mint gate is a release blocker (it is self-issuable by any authenticated principal from the day it becomes enforceable). The gate generalizes by matching its own scope constant (`any(s == <scope const>)`) while keeping the import-single-source discipline per the guardrail below; `normalize_scopes` additionally gets the per-scope `MAX_SCOPE_LEN = 128` cap in the same change (migration step 11).
- **Flag 2 — who-vs-when decision (written)**: the mint gate controls **when** (relay health), not **who** — `mint_pat` (`server/pat.rs:120-152`) requires only `AuthUser` (any authenticated principal, incl. guests and bot tokens — the extractor accepts `bot_` at `extractor.rs:130`), and the request carries **no workspace context** (`POST /api/pat`; `pat_tokens` has no workspace column, `0019_pat.sql`). A mint-time Owner/Admin check is therefore **undefined**: roles are per-workspace (`effective_member_role`, `crates/aero-storage/src/workspace/members.rs:290`; `can_administer`, `aero-common/src/workspace.rs:67`) and there is no workspace to evaluate at mint. **Decision: route-level re-authorization is the who-control.** `require_scope` is a capability check only and is explicitly *not* sufficient authorization — every future audit-gated surface must re-check principal authorization on its own context (human surfaces: `effective_member_role`/`can_administer`; machine surfaces: the scoped PAT itself, mint-gated) **in addition to** `require_scope`. Follow-up (not this change): if a workspace-agnostic audit-role concept is introduced, add the mint-time who-check then.
- **B4 — pre-existing `audit:event:write` PATs (deploy-time runbook)**: PATs minted before this change never passed the gate but will satisfy `require_scope` after deploy. Pre-deploy: (1) enumerate with `SELECT id, participant_id, name, created_at FROM pat_tokens WHERE 'audit:event:write' = ANY(scopes);` (`pat_tokens` per `0019_pat.sql`; this is the only enumeration path — `list_pat` is owner-scoped, `server/pat.rs:154`); (2) decide per row — **revoke** (owner `DELETE /api/pat/:id`, or direct `revoked_at` update) or **accept-and-audit** (record who minted/when; the token becomes gate-equivalent). Default is revoke; accept only with a written note. The same query serves F4 de-provisioning.
- **B5 — future audit ingest route must be REST**: WS auth is JWT-only — the handshake resolves via `verify_access` (`ws/ws_impl/mod.rs:546`) and has no PAT/bot branch — so a scoped-PAT audit write can never reach a WS surface. The follow-up audit-ingest route must be REST so `require_scope`-carrying PATs can authenticate.
- F4 residual: mint-time gate only; de-provisioning does not retroactively revoke minted audit PATs (operation: revoke tokens — enumerate via the B4 query above).
- No unification of the existing duplicated `SCOPE_AUDIT` const in `snaplink_commercial/http.rs:23` (out of scope; the new import is single-source for the gate).
- **Guardrail — no re-mirroring**: the mint gate imports `Q0_SQL`; it must not be replaced by a local literal copy. If a future change re-introduces a mirror, it must ship with (a) a compile-time pin `assert!(MIRROR == aero_eng::audit_provision::Q0_SQL)` (which requires the const pub — i.e. the import is available and strictly better) or a test-time text-equality pin, and (b) a documented re-sync procedure in `audit_provision.rs`'s Q0 doc comment. An `include_str!`-based pin over aero-eng's source is rejected (formatting-churn fragile, breaks on restructure).
- The other two analysis directions (in-tx audit writes; RFC 9068 claim contract into `aero-common`) remain out of scope.
- **Wire-time convention is pinned, code lands with the route**: the `aero_authz_scope_denied_total{scope}` counter + `principal_kind`-derived warn log (§3.7.2) are defined now but **not** implemented now — a standalone helper would be a zero-call builder (truth-check red; §4.4's allowlist covers only the named builders). The first route that wires `require_scope` implements the convention verbatim; deviation is a review failure.
