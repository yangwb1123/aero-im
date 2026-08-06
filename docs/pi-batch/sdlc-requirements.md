The canonical spec is at `docs/requirements/2026-08-06-message-recall-time-window.req.md` (v2 as-built) — verified current against the tree (all anchors intact: 238 migrations, predicate/fence/check sites, boundary tests at `messages.rs:769` / `recall_tests.rs:845`). Condensed specification below; no code written.

---

# Requirements Spec — Message Recall Time Window (撤回时间窗口)

**Status**: implemented & gated (spec v2 as-built; design v3; 4 reviews PASS; all gates green).

## Scope

1. `AERO_RECALL_WINDOW_SECS` — plain env (not figment `AERO__SERVER__*`): default **86400** (24h), **0 = unlimited**, invalid/unset → 86400 silent fallback (repo `env_parse` convention; boot log distinguishes fallback from intentional `0`).
2. Window anchored to `messages.created_at` (send time; edits never reset it).
3. Author recall outside the window → `409 Conflict("recall window expired")`; **room owner/admin recall exempt** (moderation path — check only fires when `actor == sender_id`).
4. Two-layer enforcement: service preflight (early 409 before the workspace rate charge — gate S1) **and** the authority re-check inside the storage transaction against the `FOR UPDATE`-locked row.
5. Inclusive boundary: allowed iff `now − created_at <= window`.
6. Docs: `.env.example`, README env table + feature matrix, `docs/recall-window.md` (sibling of `snaplink-commercial.md`).

## User stories

- **US1** author in-window: recall works exactly as before (placeholder + 已撤回).
- **US2** author expired: stable `409 conflict` / `recall window expired` over REST and WS; message stays live and untouched.
- **US3** moderation: owner/admin recalls any message at any age.
- **US4** operator: `0` disables the window (legacy behavior); unset/invalid are safe defaults; effective value logged at boot.
- **US5** race safety: boundary decided atomically under the row lock — no half-applied recall under concurrent admin recall or role change.

## Non-goals

- **No un-recall** — `recalled_at` is a one-shot terminal transition; the window never re-enables recall of a recalled message.
- No per-message/room/workspace window override (global env only); no window on `delete_message`; no expiry notifications, no UI countdown; no new error code value (reuse `conflict`; `msg` discriminates).
- **撤回 ≠ 内容彻底移除** (documented): pre-recall text remains member-visible via the history route (evidence trail); search/embedding/blobs are cleaned.

## Acceptance criteria (all implemented + green)

1. **Unit — config**: unset→86400; `"0"`→unlimited; `"60"`/`" 60 "`→60s; garbage/negative/overflow→86400 — `parse_recall_window_defaults_and_unlimited` ✅
2. **Unit — boundary** (single captured `now`): `t=window` → allowed; `t=window+1s` → expired; `0`-window any age; future `created_at`; just-sent — `recall_window_boundary_is_inclusive_and_zero_is_unlimited` ✅
3. **Admin override (storage)**: expired author → 409; same message admin → `Ok(Some)` — `recall_window_expired_author_rejected_admin_override` ✅
4. **Transaction fence (integration)**: direct `recall_outboxed_authorized` rejects expired author (no preflight in path); margins 86399s→Ok / 86401s→Err (exact instant unprovable at storage with a real clock — `now_fence − created_at = 86400s + ε`); 30-day + ZERO → Ok; author-vs-admin `tokio::join!` race → exactly one winner; expired+deleted → `"message is deleted"` (precedence); member probing expired → 403, never the window string — 6 storage tests ✅
5. **Service integration**: preflight 409 (before rate charge), admin exempt, precedence, member no-leak — 4 im-core tests ✅
6. **409 contract**: exact body `{"code":"conflict","msg":"conflict: recall window expired"}` @ 409 (thiserror Display prefix); web test uses the real envelope — `conflict_renders_thiserror_prefix_and_409` + `web/api.test.js` ✅
7. **Web**: window-expired → info toast (no hardcoded duration); already-recalled/deleted → silent; 429/5xx/0/403/404 → error toast — `web/recall_errors.test.js` ✅
8. **Gates**: check/clippy clean; `cargo test --workspace --lib` 2168 green; web-check 0 violations; truth-check 0 orphans; `test-integration.sh` 605/605; `node --test web/` 127/127; hermeticity regression (suite green with `AERO_RECALL_WINDOW_SECS=1`).

## Migration impact — none (verified, with the WHERE-predicate evaluation)

- `migrations/` = **238 before and after**; `created_at` and PK pre-exist; no new column/index/backfill.
- **The recall UPDATE needs NO new WHERE predicate — evaluated and rejected with evidence**: the row is already `FOR UPDATE`-locked in the same transaction, so the app-level check on the locked snapshot is atomic (no concurrent recall/role change can interleave between check and UPDATE). A pure-SQL alternative (`AND (sender_id = $actor OR created_at > now() − $window)`) would duplicate the role rule in SQL (owner/admin is a joined `room_members` read), mix DB-clock `now()` with the app-minted `created_at`, and add nothing over the lock. The existing fence `WHERE id=$4 AND recalled_at IS NULL AND deleted_at IS NULL` (`authorization.rs:289-306`) is unchanged.
- Ripple is signature-only: `recall_outboxed_authorized` gains `window: time::Duration` (19 test call sites updated, passing `ZERO`); `ImService` gains `recall_window` + `with_recall_window` builder (boot untouched).

## Compatibility

- **Stable 409**: same status (409), same code (`conflict`), same envelope shape (REST `{"code","msg"}` / WS `error` frame) as every pre-existing `Conflict`; the `msg` carries the identical thiserror prefix pattern. Clients keyed on status/code are unaffected.
- **Semantic caveat (shipped with the fix)**: the "409-as-success" mapping (already-recalled/deleted) is **not** applied to window-expired — surfaced as an info toast (`web/recall_errors.js`). Third-party clients that swallow 409 blindly silently no-op — safe degradation (message stays live), no data loss, no auto-retry anywhere.
- **Ordering**: the 409 is last in the failure order (after deleted/recalled/role), so no previously-stable error changes; window state is author/admin-only knowledge (no-leak pinned by tests).
