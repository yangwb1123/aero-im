# Requirements Spec — Message Recall Time Window (撤回时间窗口) [v2, as-built]

- **Module**: `aero-common` (predicate/metric) · `aero-im-core` (config + preflight) · `aero-storage` (transaction fence) · `aero-server` (contract) · `web/` (error surfacing) · docs
- **Date**: 2026-08-06 · **Status**: **implemented & gated** (v1 = spec-only; this v2 re-verifies every requirement against the shipped code)
- **Knob**: `AERO_RECALL_WINDOW_SECS` — plain env (not figment `AERO__SERVER__*`), default **86400** (24h), **0 = unlimited**, invalid/unset falls back to 86400.

---

## 1. Evidence ledger (every requirement re-verified against the working tree)

| Requirement | Verified anchor (as-built) |
|---|---|
| Config knob, plain env, default 86400 | `.env.example:149` `AERO_RECALL_WINDOW_SECS=86400`; README env table `README.md:171`; parse at `crates/aero-im-core/src/service/messages.rs:45` `parse_recall_window` (0→`Duration::ZERO`, garbage/negative/overflow→86400), env read once at `ImService::new` (`service/orig.rs:316`) + boot log (`recall_window_secs`/`recall_window_unlimited`) |
| Window anchored to send time (`created_at`) | predicate `recall_window_expired(created_at, now, window)` at `crates/aero-common/src/model/message.rs:31` (shared by both layers; `created_at` is app-minted at insert, `crud.rs:71`; never rewritten by any UPDATE) |
| Boundary: inclusive — allowed iff `age <= window`; `t=window` ok, `t=window+1s` → 409 | unit test `recall_window_boundary_is_inclusive_and_zero_is_unlimited` (`service/messages.rs:769`, single captured `now`); storage margin tests `recall_window_boundary_margins` (`message/recall_tests.rs:845`, 86399s→Ok / 86401s→Err — exact instant is unprovable at the storage layer with a real clock, by construction `now_fence − created_at = 86400s + ε > 86400s`) |
| Author-only; room owner/admin exempt (moderation path) | `existing.sender_id == actor &&` gate at both layers: preflight `messages.rs:519-526`, tx fence `message/authorization.rs:262-269`; storage test `recall_window_expired_author_rejected_admin_override` (`recall_tests.rs:804`) proves same-message admin success after author 409 |
| Fence evaluated atomically inside the storage transaction (row-locked), not only preflight | `recall_outboxed_authorized(id, actor, window, traceparent)` (`authorization.rs:206+`, `window` param `:217`) evaluates on the `FOR UPDATE`-locked snapshot (`lock_message_in_tx`, `message/events.rs:388-405`) before the body UPDATE; race test `recall_boundary_race_author_vs_admin` (`recall_tests.rs:920`) — exactly one winner, order-agnostic partition |
| Preflight 409 before the workspace rate charge (gate S1) | both transports call `assert_message_recall_preflight` before `check_ws_rate_room` (`routes/handlers/messages.rs:184`, `ws_impl/frame.rs:164`); the window check + metric emit sit inside the preflight (`messages.rs:519-526`) |
| 409 Contract: `Conflict` → 409 / code `conflict` | `#[error("conflict: {0}")]` (`aero-common/src/error.rs:22`); REST envelope `{"code","msg"}` (`aero-server/src/error.rs:26`); WS error frame (`frame.rs:523`); **wire msg carries the thiserror prefix** — `"conflict: recall window expired"` — pinned by contract test `conflict_renders_thiserror_prefix_and_409` (`aero-server/src/error.rs:68`) and web test with the real envelope (`web/api.test.js`) |
| Error precedence (window last, no leak) | service paths `messages.rs:497-527` (deleted → recalled → role → window); storage fence keeps its pre-existing role-before-state order, window appended last (`authorization.rs:245-269`); leak pins: `recall_window_no_leak_to_member` (`recall_tests.rs:998`) + `_service` (`db_tests/recall_tests.rs:419`) — member probing an expired message gets 403, never the window string |
| Metric | `names::MESSAGES_RECALL_EXPIRED_TOTAL` (`aero-common/src/metrics.rs:78`), emitted once per rejection in the preflight (single REST+WS choke point) |
| Web surface: window-expired 409 must reach the user; convergent 409s stay silent | `web/recall_errors.js` `recallErrorToast` (prefix-tolerant regex; no hardcoded duration), delegate in `web/app.js`; tests `web/recall_errors.test.js` (5) + `web/api.test.js` real-envelope test |
| Documentation deliverables | `.env.example:149`; README env table row `README.md:171`; README feature-matrix `消息生命周期` row (撤回(作者限时,房主/管理员不受限)); sibling doc `docs/recall-window.md` (sibling of `docs/snaplink-commercial.md`, same convention as its §tunables) |
| No migration | `migrations/` count **238 before and after** implementation; `created_at` column + PK already exist; the recall UPDATE's WHERE fence (`recalled_at IS NULL AND deleted_at IS NULL`, `authorization.rs:289-306`) is **unchanged** |

## 2. Scope

1. `AERO_RECALL_WINDOW_SECS`: default 86400; `0` = unlimited; invalid → 86400 (silent fallback, `env_parse` convention — a hot-path knob must not brick startup).
2. Window measured from `messages.created_at` (send time; edits do not reset).
3. Author recall outside the window → `409 Conflict("recall window expired")`; room owner/admin recall exempt.
4. Two-layer enforcement: service preflight (early 409, before rate budget) + authority re-check inside the storage transaction against the `FOR UPDATE`-locked row.
5. Inclusive boundary: allowed iff `now − created_at <= window`.
6. Docs: `.env.example`, README env table + feature matrix, `docs/recall-window.md`.

## 3. User stories

- **US1 (author, in window)** — recall within 24h works exactly as before (placeholder + `已撤回` rendering unchanged).
- **US2 (author, expired)** — recall after the window returns a stable `409 conflict` with `recall window expired` over REST and WS; the message stays live and untouched.
- **US3 (moderation override)** — room owner/admin can recall any message at any age.
- **US4 (operator)** — set `0` to disable the window (legacy behavior) or tune it; unset/invalid values are safe defaults; the effective value is logged once at boot.
- **US5 (race safety)** — a recall exactly at the boundary is decided atomically against the row lock; a concurrent admin recall or role change cannot produce a half-applied recall.

## 4. Non-goals

- **No un-recall**: `recalled_at` is a one-shot terminal transition; the window never re-enables recall of a recalled message.
- No per-message / per-room / per-workspace window override (global env only).
- No window on `delete_message` (author-only permanent delete, unchanged).
- No recipient notification about expiry, no UI countdown, no change to placeholder rendering.
- No new error code value (reuse `conflict`; the `msg` discriminates).

## 5. Acceptance criteria (all implemented + green)

1. **Unit — config parsing**: unset→86400; `"0"`→unlimited; `"60"`→60s; `" 60 "`→60s (trim); garbage/negative/overflow→86400. → `parse_recall_window_defaults_and_unlimited` ✅
2. **Unit — boundary** (single captured `now`): `t = window` → not expired; `t = window+1s` → expired; `window=0` at any age → not expired; future `created_at` → not expired; just-sent → not expired. → `recall_window_boundary_is_inclusive_and_zero_is_unlimited` ✅
3. **Admin override (storage)**: expired author → `Conflict("recall window expired")`; same message admin → `Ok(Some)` with placeholder + `recalled_by`. → `recall_window_expired_author_rejected_admin_override` ✅
4. **Transaction fence (integration)**: `recall_outboxed_authorized` called directly (no preflight in path) rejects expired author; margins 86399s→Ok / 86401s→Err; `0`-window 30-day-old message → Ok; author-vs-admin `tokio::join!` race → exactly one winner with stable Conflicts; expired+deleted → `"message is deleted"` (precedence); member probing expired → Forbidden, never the window string. → 6 storage tests ✅
5. **Service integration**: preflight 409 for expired author (before rate charge), admin exempt through preflight+recall, precedence, member no-leak. → 4 im-core tests ✅
6. **409 contract**: exact rendered body `{"code":"conflict","msg":"conflict: recall window expired"}` @ 409; web mock uses the real envelope. → `conflict_renders_thiserror_prefix_and_409` + `web/api.test.js` ✅
7. **Web**: window-expired → info toast (no hardcoded duration); already-recalled/deleted → silent; 429/5xx/0/403/404 → error toast. → `web/recall_errors.test.js` (5 tests) ✅
8. **Gates (all green on the final tree)**: `cargo check --workspace --all-targets` · `cargo clippy --workspace --all-targets -- -D warnings` · `cargo test --workspace --lib` (~1100 tests) · `scripts/web-check.sh` (0 violations) · `scripts/truth-check.sh` (0 orphans) · `scripts/test-integration.sh` (605 ignored tests on throwaway DBs) · `node --test web/*.test.js` (127/127) · hermeticity regression (suite green with `AERO_RECALL_WINDOW_SECS=1` exported).

## 6. Migration impact — none (verified)

- `migrations/` = **238 files before and after**; no new column (`.created_at` exists), no backfill, no index (PK lookup).
- **The recall UPDATE needs NO new WHERE predicate — evaluated and rejected with evidence**: the message row is already `FOR UPDATE`-locked in the same transaction before any window evaluation, so the app-level check against the locked snapshot is atomic (a concurrent recall/role change cannot interleave between check and UPDATE). A pure-SQL alternative (`AND (sender_id = $actor OR created_at > now() − $window)`) would duplicate the role rule in SQL, mix DB-clock `now()` with the app-clock-minted `created_at`, and buy nothing over the lock. The existing fence `WHERE id = $4 AND recalled_at IS NULL AND deleted_at IS NULL` (`authorization.rs:289-306`) is unchanged.
- Signature-only ripple: `recall_outboxed_authorized` gains `window: time::Duration` (19 test call sites across `recall_tests.rs` + `recall_index_fence_tests.rs` updated, all passing `Duration::ZERO` to preserve behavior); `ImService` gains `recall_window` field + `with_recall_window` builder (boot untouched).

## 7. Compatibility

- **Stable 409**: same status (409), same code value (`conflict`), same envelope shape (`{"code","msg"}` REST / `error` frame WS) as every pre-existing `Conflict`. The `msg` gains the thiserror Display prefix exactly like all other conflicts (`"conflict: recall window expired"`), so clients keyed on status/code are unaffected.
- **Semantic caveat (shipped with the fix)**: `web/api.js` documented "callers may treat 409 as success" for already-recalled/deleted — that mapping is **not** applied to window-expired. The web recall affordance surfaces it as an info toast (via `web/recall_errors.js`). Pre-existing third-party clients that swallow 409 blindly will silently no-op on an expired recall — safe degradation (message stays live), no data loss, no auto-retry anywhere.
- **Ordering**: the 409 is emitted last in the failure order (after deleted/recalled/role), so no previously-stable error changes; expired+deleted still reports "message is deleted".
