# Design — Message Recall Time Window (撤回时间窗口) [v3, as-built]

- **Module**: `aero-common` · `aero-im-core` · `aero-storage` · `aero-server` · `web/` · docs
- **Source spec**: `docs/requirements/2026-08-06-message-recall-time-window.req.md` (v2, as-built)
- **Date**: 2026-08-06 · **Status**: **implemented & gated** (v1/v2 = design-stage; this v3 re-verifies every design decision against the shipped code)
- **Scope**: config knob + two-layer window enforcement + one metric + tests + docs. **No migration, no route/frame/bus/boot change** — verified (`migrations/` count 238 before and after).

---

## §1 Verification ledger (each design claim re-checked against the implementation)

| # | Design claim | As-built verdict | Anchor |
|---|---|---|---|
| D1 | Plain env `AERO_RECALL_WINDOW_SECS` (not figment), default 86400, `0` = unlimited, invalid → default | ✅ | `.env.example:149`; `parse_recall_window` (`service/messages.rs:45`); README env row `README.md:171` |
| D2 | Read once at `ImService::new` + boot log (silent fallback diagnosable) | ✅ | `recall_window_from_env` (`messages.rs:55`) called at `orig.rs:316`; `tracing::info!` with `recall_window_secs`/`recall_window_unlimited` (`orig.rs:321-322`) |
| D3 | Additive `with_recall_window` builder (tests inject exact windows; no `set_var`) | ✅ | `orig.rs:369`; used by 4 service tests + fixture pin (`db_tests.rs:69`) |
| D4 | Shared pure predicate `recall_window_expired(created_at, now, window)`, app-clock both sides | ✅ | `aero-common/src/model/message.rs:31`, re-exported `lib.rs:47`; both call sites use `aero_common::time::now_utc()` |
| D5 | Preflight 409 after role gate, before rate charge (gate S1); metric emitted there only | ✅ | `messages.rs:511-527` (check + `MESSAGES_RECALL_EXPIRED_TOTAL`); both transports preflight before `check_ws_rate_room` (`handlers/messages.rs:184`, `frame.rs:164`) |
| D6 | Tx fence: same predicate on the `FOR UPDATE`-locked snapshot, author-only, appended last | ✅ | `recall_outboxed_authorized(id, actor, window, traceparent)` — param `authorization.rs:217`, fence `:261-269`, after role (`:234`) / deleted / recalled (`:241-247`) |
| D7 | Recall UPDATE WHERE predicate unchanged | ✅ | `WHERE id=$4 AND recalled_at IS NULL AND deleted_at IS NULL` (`authorization.rs:289-306`) — no new predicate (see §3) |
| D8 | 409 contract: `Conflict` → 409/`conflict`; wire msg carries thiserror prefix | ✅ | `#[error("conflict: {0}")]` (`error.rs:22`); envelopes `aero-server/src/error.rs:26`, `frame.rs:523`; contract test `conflict_renders_thiserror_prefix_and_409` (`aero-server/src/error.rs:68`) |
| D9 | Metric `aero_messages_recall_expired_total`, preflight-only emit | ✅ | `metrics.rs:78`; single emit at `messages.rs:525` |
| D10 | Boundary: unit-level exact `t=window` (captured clock); storage margins | ✅ | `recall_window_boundary_is_inclusive_and_zero_is_unlimited` (`messages.rs:769`); `recall_window_boundary_margins` (`recall_tests.rs:845`) |
| D11 | Web: pure `recallErrorToast`, prefix-tolerant, no hardcoded duration | ✅ | `web/recall_errors.js:16`; delegate `web/app.js`; tests `web/recall_errors.test.js` + `web/api.test.js` (real envelope) |
| D12 | Fixture hermeticity: shared `service()` pins ZERO | ✅ | `db_tests.rs:69` `.with_recall_window(time::Duration::ZERO)`; regression gate green with `AERO_RECALL_WINDOW_SECS=1` exported |
| D13 | Zero migration | ✅ | `migrations/` = 238 before/after; `created_at` + PK pre-exist |

**Deviations adopted during implementation (all gate-approved, evidence-backed)**:
1. Predicate lives in **`aero-common`** (v2 design showed it in `service/messages.rs`) — the storage fence cannot import im-core's `pub(crate)` fn; `model/message.rs` is the shared leaf home. `parse_recall_window`/`recall_window_from_env` stayed in im-core (config concern, mirrors moderator seam).
2. **`from_sink` test ctor** (`orig.rs:659`) pins `recall_window: ZERO` — second `ImService` initializer discovered at build time (E0063).
3. Boot log carries both `recall_window_secs` **and** `recall_window_unlimited` — an explicit flag so `0` (intentional disable) is distinguishable from a typo'd fallback (security-review F1).

## §2 Config plumbing

**Knob**: `AERO_RECALL_WINDOW_SECS`, plain single-underscore env — same class as `AERO_RATE_LIMIT_PER_SEC`, `AERO_BLOCKED_WORDS`, snaplink envs. **Not** figment `AERO__SERVER__*`, **not** in `config.toml`/`config.example.toml` (snaplink precedent: `.env.example` only).

**Precedent check — `AERO_SNAPLINK_REQUEST_TIMEOUT_SECS`** (verified): wired in `crates/aero-server/src/snaplink_commercial/config.rs:246` via `duration_secs("AERO_SNAPLINK_REQUEST_TIMEOUT_SECS", 10, 1, 120)` → `integer_env(name, default, min, max)` (`:486-501`) which **bails fail-loud** (`bail!("{name} must be between {min} and {max}")`) on invalid/out-of-range; documented at `.env.example:96`. The recall window **deliberately diverges** to the server-config `env_parse(...).unwrap_or(default)` convention (silent fallback, `aero-server/src/config.rs:168-170`): it is a non-security hot-path knob, not a boot-time contract — a typo must not brick startup, and the boot log (D2) makes the fallback diagnosable.

```rust
// service/messages.rs — pure parse (unit-testable, no env mutation)
pub(crate) const RECALL_WINDOW_DEFAULT_SECS: i64 = 86_400;
pub(crate) fn parse_recall_window(raw: Option<&str>) -> time::Duration {
    match raw.and_then(|v| v.trim().parse::<i64>().ok()) {
        Some(secs) if secs >= 0 => time::Duration::seconds(secs), // 0 == unlimited
        _ => time::Duration::seconds(RECALL_WINDOW_DEFAULT_SECS),
    }
}
pub(crate) fn recall_window_from_env() -> time::Duration {
    parse_recall_window(std::env::var("AERO_RECALL_WINDOW_SECS").ok().as_deref())
}
```

`ImService::new` (`orig.rs:316`) reads it once; `with_recall_window` (`orig.rs:369`) is the test seam. **Zero boot change** (`bin/boot/services.rs:75` untouched).

## §3 Preflight vs row-locked transaction fence

### 3.1 The predicate (single source of truth, both layers)

```rust
// aero-common/src/model/message.rs
pub fn recall_window_expired(created_at: OffsetDateTime, now: OffsetDateTime, window: time::Duration) -> bool {
    window != time::Duration::ZERO && now - created_at > window
}
```

Inclusive boundary: allowed iff `now − created_at <= window`; expired iff strictly older. Both layers use `aero_common::time::now_utc()` (app clock — `created_at` is app-minted at insert, `crud.rs:71`, so DB-clock `now()` would mix clocks and fuzz the boundary).

### 3.2 Preflight (UX + rate-gate fairness)

`assert_message_recall_preflight` (`messages.rs:455-527`): get → room access → deleted → already-recalled → role gate (`recall_authorized`) → **window check** (author-only via `sender_id == actor`; admin/owner exempt) → metric emit → 409. Placement *after* the role gate closes the window-state oracle (a plain member probing gets 403, never the window string — pinned by two leak tests) and *before* `check_ws_rate_room` implements gate S1 (doomed attempts never burn the workspace budget). REST handler (`handlers/messages.rs:184`) and WS frame (`frame.rs:164`) both take this path.

### 3.3 Transaction fence (authority) — exact SQL predicate

`recall_outboxed_authorized(id, actor, window, traceparent)` (`authorization.rs:206-269`) — the only window authority:

| Layer | SQL / expression | Change |
|---|---|---|
| Row lock | `SELECT id, room_id, sender_id, …, created_at, …, version FROM messages WHERE id = $1 FOR UPDATE` (`message/events.rs:388-405`) | unchanged |
| Window (app-level on the locked row) | `expired ⟺ window ≠ 0 ∧ now_utc() − created_at > window` evaluated after the role/deleted/recalled gates (`authorization.rs:261-269`) | new |
| Terminal-state fence | `UPDATE messages SET … WHERE id = $4 AND recalled_at IS NULL AND deleted_at IS NULL` (`authorization.rs:289-306`) | **unchanged — no new WHERE predicate** |

**Why no SQL predicate on the UPDATE (evaluated and rejected with evidence)**: the row is already `FOR UPDATE`-locked in the same transaction before any window evaluation, so the app-level check on the locked snapshot is atomic — no concurrent recall or role change can interleave between check and UPDATE. A pure-SQL alternative (`AND (sender_id = $actor OR created_at > now() − $window::interval)`) would (a) duplicate the role rule (owner/admin is a joined `room_members` read, not a column), (b) mix DB clock with the app-minted `created_at`, making the inclusive boundary unprovable, and (c) add nothing over the existing lock. The storage fence's pre-existing **role-before-state** order (vs the service's state-before-role) is deliberately preserved — only the window check is appended last in both layers, keeping the documented precedence (404 → 403 access → 409 deleted → 409 recalled → 403 role → 409 window).

## §4 409 error contract

- REST: `HTTP 409` + `{"code":"conflict","msg":"conflict: recall window expired"}` — automatic via `Error::Conflict` Display (`error.rs:22`) → `aero-server/src/error.rs:26`.
- WS: `{"type":"error","code":"conflict","msg":"conflict: recall window expired"}` — automatic (`frame.rs:523`).
- **The `msg` carries the thiserror prefix** `"conflict: "` (same as every Conflict) — the web discriminator is prefix-tolerant (`/recall window expired/.test(...)`), and the server contract test (`error.rs:68`) pins the exact rendered body so a reworded variant can never silently break the client.
- Precedence: window check is last; expired+deleted still reports "message is deleted"; the window string is author/admin-only knowledge.
- No auto-retry anywhere; "already recalled/deleted" 409s stay convergent-silent; window-expired 409 is surfaced as an info toast (no hardcoded duration — the knob is operator-tunable).

## §5 Metrics

`names::MESSAGES_RECALL_EXPIRED_TOTAL = "aero_messages_recall_expired_total"` (`metrics.rs:78`), emitted **only** in `assert_message_recall_preflight` (`messages.rs:525`) — the single choke point shared by REST and WS (a `recall_message` emit would count ~zero since the preflight 409s first; storage emits no metrics, so the boundary-race fraction is uncounted by design — documented in `docs/recall-window.md`). Unlabeled counter, complements `aero_messages_recalled_total` (successes) as the US4 tuning signal. Note: an in-room author can bump it pre-rate-gate (≤20 rps, bounded) — alert thresholds must tolerate that (documented).

## §6 Tests (all implemented, all green)

| Layer | Test | Covers |
|---|---|---|
| Unit (no DB) | `parse_recall_window_defaults_and_unlimited` (`messages.rs:755`) | unset/0/60/`" 60 "`/garbage/negative/overflow |
| Unit (no DB) | `recall_window_boundary_is_inclusive_and_zero_is_unlimited` (`messages.rs:769`) | exact `t=window` (single captured `now`), `t=window+1s`, ZERO at 365d, future `created_at`, just-sent |
| Storage DB | `recall_window_expired_author_rejected_admin_override` (`recall_tests.rs:804`) | author 409 + same-message admin `Ok(Some)` (placeholder, `recalled_by`, audit, outbox) |
| Storage DB | `recall_window_boundary_margins` (`:845`) | 86399s→Ok / 86401s→Err (latency-proof; exact instant unprovable at this layer) |
| Storage DB | `recall_window_zero_is_unlimited` (`:892`) | 30-day-old message, ZERO window |
| Storage DB | `recall_boundary_race_author_vs_admin` (`:920`) | `tokio::join!` — exactly one winner, order-agnostic stable Conflicts, one outbox row |
| Storage DB | `recall_window_precedence_deleted_before_expired` (`:965`) | expired+deleted → "message is deleted" |
| Storage DB | `recall_window_no_leak_to_member` (`:998`) | member probing expired → Forbidden, never the window string |
| Service DB | `recall_preflight_window_expired_author` / `_admin_exempt` (`db_tests/recall_tests.rs:322,367`) + `_no_leak_to_member_service` (`:419`) + `_precedence_deleted_before_expired_service` (`:454`) | full path incl. preflight-before-rate-charge ordering and admin exemption |
| Server | `conflict_renders_thiserror_prefix_and_409` (`error.rs:68`) | exact 409 body `{"code":"conflict","msg":"conflict: recall window expired"}` |
| Web | `web/recall_errors.test.js` (5) + `web/api.test.js` (real-envelope 409) | toast mapping; msg survives `request()` extraction |

Backdating technique (F5, gate-approved): `UPDATE messages SET created_at = $1 WHERE id = $2` with an **app-clock parameter-bound** timestamp (`now_utc() − Duration::seconds(...)`) — never DB `now() - interval` (clock mixing; proven pattern `notification_bundle.rs:640-680`). Fixture hermeticity: shared `service()` pins `with_recall_window(ZERO)`; window tests build `service(pool).with_recall_window(...)`; regression gate runs the suite with `AERO_RECALL_WINDOW_SECS=1` exported.

**Gate results (final tree)**: `cargo check --workspace --all-targets` clean · `cargo clippy --workspace --all-targets -- -D warnings` clean · `cargo test --workspace --lib` 2168 green (design doc v2 said ~1100 — undercount, direction safe) · `scripts/web-check.sh` 0 violations · `scripts/truth-check.sh` 0 orphans · `scripts/test-integration.sh` 605/605 (throwaway DBs, dropped) · `node --test web/*.test.js` 127/127 · hermeticity regression green. Independently re-executed by the QA reviewer plus a live E2E boot (`AERO_RECALL_WINDOW_SECS=5`): exact 409 envelope, admin exemption, member no-leak, row immutability, metric semantics (`aero_messages_recall_expired_total=4` exactly, 0 member probes).

## §7 Change radius (as-built)

| File | Change |
|---|---|
| `crates/aero-common/src/model/message.rs` | +`recall_window_expired` (shared predicate) |
| `crates/aero-common/src/lib.rs` | re-export |
| `crates/aero-common/src/metrics.rs` | +`MESSAGES_RECALL_EXPIRED_TOTAL` |
| `crates/aero-im-core/src/service/messages.rs` | +`RECALL_WINDOW_DEFAULT_SECS`/`parse_recall_window`/`recall_window_from_env`; preflight window check + metric; `recall_message` check; passes `self.recall_window` to storage; doc path 6; unit tests |
| `crates/aero-im-core/src/service/orig.rs` | +field `recall_window`; env read + boot log in `new()`; +`with_recall_window`; `from_sink` pins ZERO |
| `crates/aero-storage/src/message/authorization.rs` | `recall_outboxed_authorized` +`window` param; in-tx fence after recalled-at |
| `crates/aero-storage/src/message/recall_tests.rs` | 15 call sites pass `Duration::ZERO`; +6 window tests |
| `crates/aero-storage/src/message/recall_index_fence_tests.rs` | 4 call sites pass `Duration::ZERO` |
| `crates/aero-im-core/src/db_tests.rs` | fixture pins `with_recall_window(ZERO)` |
| `crates/aero-im-core/src/db_tests/recall_tests.rs` | +4 service window tests |
| `crates/aero-server/src/error.rs` | +contract test |
| `crates/aero-server/src/routes/handlers/messages.rs` | doc: stable failures + window-expired |
| `web/recall_errors.js` / `web/recall_errors.test.js` | **new** — pure toast mapping + tests |
| `web/app.js` / `web/api.js` / `web/api.test.js` | delegate, comment fix, real-envelope test |
| `.env.example` / `README.md` / `docs/recall-window.md` | env row + feature-matrix + sibling doc |

**Explicitly untouched (verified)**: migrations (238 before/after), `recall_locked_outboxed_in_tx` (rejection-free body), route table, WS frame schema, `RoomEvent`/outbox payloads, hub/bus, `config.toml`, boot, and the storage fence's pre-existing role-before-state order.
