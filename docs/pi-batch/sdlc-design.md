Design doc v3 is current and verified (all sections present; snaplink anchor `config.rs:246/494`; env read `messages.rs:56`). Condensed design below — no code written.

---

# Design — Message Recall Time Window (v3 as-built, verified)

**Status**: implemented & gated (13/13 design assertions verified against shipped code; DB/QA/compliance/security reviews PASS; gate PASS). Canonical: `docs/design/2026-08-06-message-recall-time-window.design.md`.

## §1 Config plumbing

- **Knob**: `AERO_RECALL_WINDOW_SECS`, plain single-underscore env — same class as `AERO_RATE_LIMIT_PER_SEC`/`AERO_BLOCKED_WORDS`/snaplink envs. **Not** figment `AERO__SERVER__*`, **not** in `config.toml` (snaplink precedent: `.env.example` only).
- **Precedent checked — `AERO_SNAPLINK_REQUEST_TIMEOUT_SECS`**: wired via `duration_secs(name, default, min, max)` → `integer_env` (`snaplink_commercial/config.rs:246,494-501`) which **bails fail-loud** on invalid/out-of-range. The recall window **deliberately diverges** to the `env_parse(...).unwrap_or(default)` silent-fallback convention (`aero-server/src/config.rs:168-170`): non-security hot-path knob, must not brick startup; the boot log makes fallback diagnosable.
- **Wiring**: pure `parse_recall_window(Option<&str>) -> time::Duration` (0→ZERO; garbage/negative/overflow→86400; trim-tolerant) + thin `recall_window_from_env()` (`messages.rs:45-57`); read **once** at `ImService::new` (`orig.rs:316`) with `tracing::info!` (`recall_window_secs`/`recall_window_unlimited` — distinguishes intentional `0` from typo-fallback); `with_recall_window` builder (`orig.rs:369`) as the test seam. **Zero boot change.**

## §2 Preflight vs row-locked transaction fence — exact SQL predicate

**Shared predicate** (`aero-common/src/model/message.rs:31`): `recall_window_expired(created_at, now, window) = window ≠ 0 ∧ now − created_at > window` — inclusive boundary (allowed iff `age <= window`); both layers use `aero_common::time::now_utc()` (app clock; `created_at` is app-minted, `crud.rs:71` — DB `now()` would mix clocks).

| Layer | SQL / expression | Change |
|---|---|---|
| Row lock | `SELECT …, created_at, … FROM messages WHERE id = $1 FOR UPDATE` (`message/events.rs:388-405`) | unchanged |
| Preflight (`assert_message_recall_preflight`, `messages.rs:455-527`) | Rust check after role gate, before `check_ws_rate_room` (gate S1, both transports: `handlers/messages.rs:184`, `frame.rs:164`) + metric emit | new |
| Tx fence (`recall_outboxed_authorized(id, actor, window, …)`, `authorization.rs:206-269`) | same predicate on the **locked snapshot**, author-only `sender_id == actor` (admin/owner exempt), appended **last** after role→deleted→recalled | new |
| Terminal fence | `UPDATE messages SET … WHERE id = $4 AND recalled_at IS NULL AND deleted_at IS NULL` (`authorization.rs:289-306`) | **unchanged — no new WHERE predicate** |

**Why no SQL predicate (evaluated, rejected with evidence)**: the `FOR UPDATE` row lock already serializes, so the app-level check on the locked snapshot is atomic; a SQL alternative (`AND (sender_id = $actor OR created_at > now() − $window)`) would duplicate the role rule (owner/admin is a joined `room_members` read), mix DB/app clocks, and add nothing over the lock. The storage fence's pre-existing **role-before-state** order is deliberately preserved (service is state-before-role); only the window check is appended last in both layers. Precedence: 404 → 403 access → 409 deleted → 409 recalled → 403 role → **409 window**. Window state is author/admin-only (no-leak pinned by 2 tests).

## §3 409 error contract

- REST: `409` + `{"code":"conflict","msg":"conflict: recall window expired"}` — automatic via `Error::Conflict` Display `#[error("conflict: {0}")]` (`error.rs:22`) → `aero-server/src/error.rs:26`.
- WS: `{"type":"error","code":"conflict","msg":"conflict: recall window expired"}` (`frame.rs:523`).
- **The `msg` carries the thiserror prefix** — web discriminator is prefix-tolerant (`/recall window expired/`); server contract test pins the exact body (`error.rs:68`); web test mocks the real envelope. Window-expired is **not** in the "409-as-success" mapping — info toast via `web/recall_errors.js` (no hardcoded duration). No auto-retry anywhere.

## §4 Metrics

`aero_messages_recall_expired_total` (`metrics.rs:78`), emitted **only** in `assert_message_recall_preflight` (`messages.rs:525`) — the single REST+WS choke point (a `recall_message` emit would count ~zero; storage emits no metrics, so the boundary-race fraction is uncounted by design, documented). Unlabeled; complements `aero_messages_recalled_total` as the US4 tuning signal. Documented caveat (gate-resolved): the "≤20 rps" bound is REST-only; WS post-upgrade frames are unthrottled — same class as pre-existing edit preflight; frame-budget hardening is backlog.

## §5 Tests (all implemented, all green)

- **Unit (no DB)**: `parse_recall_window_defaults_and_unlimited` (unset/0/60/trim/garbage/negative/overflow); `recall_window_boundary_is_inclusive_and_zero_is_unlimited` — exact `t=window` with a **single captured `now`** (the only place the exact instant is provable), `t=window+1s`, ZERO at 365d, future `created_at`, just-sent.
- **Storage DB (6)**: admin override (`recall_window_expired_author_rejected_admin_override`); margins 86399s→Ok / 86401s→Err (`recall_window_boundary_margins` — latency-proof, `now_fence − created_at = 86400s + ε`); ZERO-unlimited at 30d; author-vs-admin `tokio::join!` race (`recall_boundary_race_author_vs_admin` — exactly one winner, order-agnostic stable Conflicts); precedence (`recall_window_precedence_deleted_before_expired`); member no-leak (`recall_window_no_leak_to_member`).
- **Service DB (4)**: preflight 409 for expired author, admin exempt, precedence, member no-leak. **Server**: `conflict_renders_thiserror_prefix_and_409`. **Web (6)**: `recall_errors.test.js` (5) + `api.test.js` real-envelope.
- **Technique**: backdate via `UPDATE messages SET created_at = $1` with **app-clock parameter-bound** timestamp (never DB `now() - interval`); shared fixture pins `with_recall_window(ZERO)` (hermeticity — suite green with `AERO_RECALL_WINDOW_SECS=1` exported).
- **Gate results**: check/clippy clean · 2168 lib · 605 integration · 127 web · web-check 0 · truth-check 0 · hermeticity regression ✓ · QA live E2E (exact 409, admin exempt, member 403, metric=4, 0 member probes) ✓.

## §6 Change radius (as-built)

13 modified + 2 new files: `aero-common` (predicate `model/message.rs`, re-export, metric const) · `aero-im-core` (`messages.rs` helpers/preflight/recall_message + unit tests; `orig.rs` field/env-read/boot-log/builder; `db_tests.rs` fixture pin; `db_tests/recall_tests.rs` +4) · `aero-storage` (`authorization.rs` param+fence; `recall_tests.rs` 15 call sites +6; `recall_index_fence_tests.rs` 4 call sites) · `aero-server` (`error.rs` contract test; handler doc) · `web/` (`recall_errors.js`+test new; `app.js`/`api.js`/`api.test.js`) · docs (`.env.example`, `README.md`, `docs/recall-window.md` new).

**Untouched (verified)**: migrations (238 before/after), `recall_locked_outboxed_in_tx` body, route table, WS frame schema, `RoomEvent`/outbox payloads, hub/bus, `config.toml`, boot, storage's role-before-state order.

**Gate-approved deviations from v2 design**: predicate relocated to `aero-common` (storage can't import im-core); `from_sink` test ctor pins ZERO (E0063 at build); dual-field boot log. **Accepted backlog** (non-blocking): WS frame budget, committed E2E smoke, metric-assert harness, demotion-race test, 0238 concurrent index rebuild (prerequisite migration, scale-gated), existence oracle (rejected as fix — repo-wide contract).
