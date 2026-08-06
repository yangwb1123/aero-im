# Design — DB-gated tests for uncovered notification fan-out sources & relay failure paths

- **Module**: `crates/aero-im-core` (test-only; zero production-code changes in any crate)
- **Source spec**: `docs/requirements/2026-08-06-aero-im-core-relay-failure-fanout-tests.req.md` (stage PASS)
- **Date**: 2026-08-06 · **Status**: design (unimplemented)
- **Scope**: `#[ignore]`-gated Postgres integration tests closing the three gaps the direction names: (1) user-group `@handle` + keyword-alert fan-out (`dispatch_notifications` branches production-wired at boot, zero test coverage), (2) the durable relay loops' failure branches (`mark_failed` re-park, partial-batch count, superseded-lease `Ok(false)` fencing, side-effect error re-park), (3) `flush_notification_bundles` (timer-wired, zero test call sites). Touched files only: `db_tests.rs`, `db_tests/notifications_tests.rs`, `db_tests/relay_tests.rs`, `test_util.rs` (all `cfg(test)`).

---

## §1 Verification ledger (evidence was untrusted; every claim re-checked against the working tree)

| # | Evidence claim | Verdict | Exact anchor |
|---|---|---|---|
| V1 | `dispatch_notifications` at orig.rs:652; user-groups branch :741–764, keyword-alerts :766–790 | ✅ | `service/orig.rs:652` `pub(crate) async fn dispatch_notifications`. Reply-author :675–683, direct mentions :685–692, thread subs :688–712, broadcasts :730–758, **user groups :741–764** (`groups.resolve(workspace, handle)` → `groups.members(group.id)` → `or_insert(Mention)`), **keyword alerts :766–790** (`alerts.matching_subscribers(workspace, &text)` → `or_insert(Mention)`), block filter :818–832, workspace mute :834–867, per-prefs batch filter :927–965, mention/reply partition :967–979, immediate `insert_many_outboxed` :982–997, bundle `insert_many_idempotent` :999–1016 |
| V2 | `notification_service()` (db_tests.rs:103–127) wires `thread_subs` but no `with_user_groups`/`with_keyword_alerts` | ✅ | `db_tests.rs:103–127`; wires workspaces/notifications/prefs/blocks/workspace_mutes/thread_subs/thread_mutes/thread_notification_prefs (+bundles iff `bundles`). No user-groups/keyword-alerts. `rg` over the whole test tree: zero hits |
| V3 | 14 tests in `notifications_tests.rs`, none for groups/keywords | ✅ | exactly 14 `#[tokio::test]` (at1:56, at2:135, at2b:203, at3:265, at4a:385, at4b:406, at4c:428, at5a:478, at5b:499, at5c:523, at5d:549, at5e:573, at5f:597, at5g:630; the 15th `async fn` is the `try_presence_store` helper); zero `group`/`keyword`/`@handle` hits |
| V4 | `finish_claimed_outbox` (outbox.rs:57) three-arm `mark_failed` | ✅ | `service/outbox.rs:57`; error branch :61–95: `Err(mark_error)` warn / `Ok(true)` "retry scheduled" / `Ok(false)` debug "superseded lease". `mark_failed` at `event_outbox.rs:476`: `SET available_at = now + outbox_backoff_delay(attempts), claimed_at = NULL, last_error = $4 WHERE id AND attempts = $2 AND claimed_at IS NOT NULL AND published_at IS NULL`; returns `rows_affected() > 0` |
| V5 | `finish_message_side_effect` (side_effects.rs:48) three-arm | ✅ | `service/side_effects.rs:48` (spans :48–93); identical arms; `mark_failed` at `message_side_effect.rs:307` (WHERE additionally `completed_at IS NULL`); `side_effect_backoff` :358 (1s·2^attempts, cap 300) |
| V6 | `relay_tests.rs` at6a/at6b/at7 success-only | ✅ | at6a:29, at6b:133, at7:191 — all `#[ignore]`-gated, happy-path/stale-skip; no failure injection anywhere |
| V7 | `flush_notification_bundles` (orig.rs:1059) zero test call sites | ✅ | `orig.rs:1059`; `rg` outside orig.rs → zero hits in `db_tests*` and submodules |
| V8 | production flush timer at background.rs:459 | ✅ | `crates/aero-server/src/bin/boot/background.rs:459` `im.flush_notification_bundles().await;` (timer :443–461) |
| V9 | boot wiring at services.rs:86/:88 (spec C1) | ✅ | `services.rs:86` `.with_user_groups(aero_storage::UserGroupRepo::new(deps.pg.clone()))`, `:88` `.with_keyword_alerts(...)` (also :87 `with_message_edits`) — the direction's :89–91 was 3 lines off; claim stands |
| V10 | `message_side_effect_jobs.message_id` has no FK (migration 0165) | ✅ | `migrations/0165_message_side_effect_jobs.sql` — `message_id UUID NOT NULL` (plain; no REFERENCES) + `UNIQUE (message_id, mutation_version, kind)`. Nonexistent-id jobs are insertable → deterministic error-re-park injection |
| V11 | `MockBus` has no failure knob; `publish_idempotent` default delegates to `publish` | ✅ | `test_util.rs:16–46` (`published: Mutex<Vec<(String, Vec<u8>)>>`, `subscribe` unimplemented); `aero-bus/src/traits.rs` default `publish_idempotent` → `self.publish(...)`; `service/events.rs:28` `publish_bytes_idempotent` → `EventBus::publish_idempotent`. Erroring `publish` reaches the `mark_failed` branch with no other seam |
| V12 | flush deadline env-gated | ✅ | `notification_bundle.rs:45–54` `AERO_BUNDLE_DEADLINE_SECS` (default 30s, floor 10s), read at repo **construction**. Tests must backdate `created_at` via SQL (proven pattern `notification_bundle.rs:640–680`); in-test env mutation is process-global and parallel-unsafe |
| V13 | backoff values assertable (1s/2s, cap 300s) | ✅ | `event_outbox.rs:17` `BASE_BACKOFF_SECONDS = 1`, `outbox_backoff_delay` :520; `message_side_effect.rs:358` `side_effect_backoff` |
| V14 | `attempts` increments at claim time (spec C3) | ✅ | `event_outbox.rs:338` `attempts = outbox.attempts + 1` inside `claim_due` (`SET claimed_at = $1, attempts = outbox.attempts + 1`); same at :395; `message_side_effect.rs claim_matching` likewise. After one failed dispatch a row shows `attempts = 1` |
| V15 | flush outbox `event_id` is `bundle_delivery_id` (spec C2) | ✅ | `notification_bundle.rs:36` `BUNDLE_DELIVERY_NAMESPACE = uuid!("37301db9-9690-4ad7-8324-12d5cdd730a0")`; `bundle_delivery_id(&bundle_ids, pid, room, thread_root)` :450–462 = `Uuid::new_v5(namespace, "{pid}:{room}:{root}:{sorted-ids-with-colons}")`; flush inserts the Notify outbox row via `EventOutboxRepo::insert_room_event_in_tx(..., Some(delivery_id))` (:355–367, idempotent `ON CONFLICT (event_id)`) with payload `RoomEvent::NotifyBatch { room_id, message_id, by: actor, delivery_id, recipients: vec![NotifyTarget{participant, kind}] }`. `notify_delivery_id` (orig.rs:89, namespace `6f1d2e3c-…`) is the **bundle row's `delivery_id` column** and the immediate-path outbox id — two distinct ids, asserted separately (AT-4) |
| V16 | side-effect claim completes at bundle-insert time, not flush | ✅ | `insert_many_idempotent(..., Some(source))` → `complete_bundle_source` (:420) commits the claim inside the bundle-insert tx; `complete_source_claim_in_tx` → `MessageSideEffectRepo::complete_in_tx(tx, id, attempts, now)` |
| V17 | prior approved design exists; prior suite uncommitted (spec C4) | ✅ | `docs/auto/runs/add-db-gated-integration-tests-for-the-notificat-d779ddbf/artifacts/design-a77de8a6/task-1-design.md` (gate PASS); `git status`: `db_tests.rs` modified + `db_tests/{notifications,relay}_tests.rs` untracked; all three current gap claims re-verified against the working tree |
| V18 | DND full-day window `(0, 1440)` | ✅ | `set_dnd` no range validation (`notification_prefs.rs:276`); `in_dnd_window` end-exclusive → `(0, 1439)` has a 23:59 UTC hole; `(0, 1440)` covers all 1440 minutes (prior design D3, re-confirmed) |
| V19 | fan-out stores are pub-exported and callable from aero-im-core tests | ✅ | `aero-storage/src/lib.rs:271/274` `pub use keyword_alert::{…, KeywordAlertRepo}` / `pub use user_group::{…, UserGroupRepo}`; `create_authorized(ws, handle, name, caller)` :182 (caller needs a workspace role — `new_participant` enrolls into nil workspace as `WorkspaceRole::Member`, db_tests.rs:309–330); `add_member_authorized(ws, group, participant, caller)` :295; `resolve` :600; `members` :741; `add_authorized(participant, ws, keyword)` :99 (asserts effective workspace membership); `matching_subscribers(ws, text)` :312 (case-insensitive `position(keyword IN lower(text))`, DISTINCT, effective-access filter) |

**New load-bearing facts discovered at design time (all deterministic, none contradict the spec):**

1. **F1 — cross-test pending-row pollution breaks exact batch counts.** `send_message`'s fast path dispatches **only the Message aggregate outbox row** (`dispatch_event_outbox_id(outbox_id)`, messages.rs:184–191); the Notify row stays pending for the batch (at6a's self-seeded precondition pattern). Consequences: (a) every earlier test (at1, at2, at3-immediate, at4a/b/c, at5a–g) leaves a pending Notify row in the shared throwaway DB; (b) at6a's pump drains those (they are older → claimed first, `available_at ASC, created_at ASC, id ASC`); (c) **at6b leaves exactly one new pending row** (its side-effect dispatch writes a Notify outbox row it never relays). AT-3b's exact `Ok(1)` and AT-3c's gate-targeting therefore need a **drain-to-quiescence** precondition (helper `drain_pending_outbox`, §3.2) — without it, the single injected failure in AT-3b hits at6b's older row first and the count comes back `Ok(2)`.
2. **F2 — "MockBus recorded zero publishes" must be a snapshot delta.** Seeding records the Message frames (fast path, knob disarmed). Assert `bus.published.len()` unchanged since the post-seed snapshot, never an absolute zero.
3. **F3 — `NotificationRepo::list` is participant-scoped** (`list(participant, before, unread_only, limit)`, notification.rs:304), not message-scoped. AT-1/AT-2 kind+actor assertions need a new message-scoped raw-SQL helper `notification_rows` (pattern already used inline by at3 for bundles).
4. **F4 — AT-3d creates a globally-claimable due side-effect job** (same class as AT-2/AT-6b/AT-7) → its seed→dispatch→assert section holds `BATCH_SERIAL` (db_tests.rs:71–75) per the established convention. AT-3a–3c claim the **outbox** batch only — they follow at6a's no-lock pattern (exactness restored by drain + runbook `--test-threads=1`).
5. **F5 — MockBus knob shape**: tokio dev-deps already carry `sync` (`crates/aero-im-core/Cargo.toml` dev-deps: `tokio = { workspace = true, features = ["macros", "rt", "rt-multi-thread", "sync"] }`) → `tokio::sync::oneshot` rendezvous is available with zero dependency changes. The gate uses **two oneshots** (entered/release), not a barrier/notify: if the test panics while blocked, the release sender drops → the blocked `publish` gets a `RecvError` and returns the injected error — no hang by construction.
6. **F6 — flush payload shape is exact**: `by` = `last.actor_id.unwrap_or(sender_id)` (the reply sender), `recipients` is a 1-element vec `[NotifyTarget { participant, kind }]` — AT-4 decodes the pending outbox payload against exactly this struct (`notification_bundle.rs:346–356`).
7. **F7 — kind tokens**: `NotificationKind::as_str` = `"mention"`/`"reply"`/`"aggregate_reply"` (aero-common model) — assertable strings in `notification_rows`.
8. **F8 — claim order is deterministic per run** (`available_at, created_at, id` ASC) but AT-3b must not assert *which* seeded row fails first (spec: "not asserted") — assert the state partition instead.

---

## §2 Design overview

Zero production-code changes, zero new dependencies, zero schema changes, zero migration edits. Two test files gain tests; the harness and MockBus gain additive test-only surface:

```
crates/aero-im-core/src/test_util.rs                MockBus failure knobs (fail_publishes + rendezvous gate)
crates/aero-im-core/src/db_tests.rs                 notification_service() + user_groups/keyword_alerts; new helpers
crates/aero-im-core/src/db_tests/notifications_tests.rs   AT-1a…1e (groups), AT-2 (keywords), AT-4 (flush)
crates/aero-im-core/src/db_tests/relay_tests.rs           AT-3a…3d (failure injection)
```

Every test drives the **production orchestration path**: `send_message` (messages.rs:75) → `kick_message_side_effects` (messages.rs:554, awaited under `cfg(test)` → deterministic completion) → `dispatch_message_side_effects_for` (side_effects.rs:31) → `dispatch_notifications` (orig.rs:652) → outbox/bundle writes → `dispatch_event_outbox_batch` (outbox.rs:27) → `publish_bytes_idempotent` (events.rs:28) → `MockBus::publish`. Raw SQL is used **only** to seed state the public API cannot create (nonexistent-message jobs, mutes, backdating) and to assert row state. The failure knobs are armed **after** seeding so setup publishes succeed.

---

## §3 API changes

### 3.1 Production API — none

No public, `pub(crate)`, or `pub` signature in any crate changes. No `Cargo.toml` edits (tokio `sync`, `uuid`, `time`, `serde_json` already available). No migrations.

### 3.2 Test-only surface (new, all `cfg(test)`-reachable)

**`test_util.rs` — `MockBus` failure knobs** (additive; `Default` keeps existing call sites working):

```rust
pub(crate) struct MockBus {
    pub published: Mutex<Vec<(String, Vec<u8>)>>,
    /// Fail the next N publish calls with BusError::Nats("injected failure")
    /// (not recorded). usize::MAX ≈ "always errors". Decrement is atomic and
    /// saturating; armed after seeding, disarmed (0) during setup.
    fail_publishes: std::sync::atomic::AtomicUsize,
    /// Rendezvous gate (AT-3c only): when armed, a failing publish signals
    /// `entered` and awaits `release` before returning the injected error.
    gate: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}
impl MockBus {
    pub fn arm_failures(&self, n: usize);
    pub fn arm_gate(&self, entered: oneshot::Sender<()>, release: oneshot::Receiver<()>);
}
```

`publish` logic: `fetch_update` decrement → if a failure was consumed: take gate sender (if any) → `entered.send(())` → await `release` (a dropped sender yields `RecvError` → proceed to fail — panic-safe, F5) → `return Err(BusError::Nats("injected failure"))` without recording. Otherwise record and `Ok(())` as today.

**`db_tests.rs` — `notification_service()` (REQ-0)**: add `.with_user_groups(UserGroupRepo::new(pool.clone()))` and `.with_keyword_alerts(KeywordAlertRepo::new(pool.clone()))` mirroring boot (services.rs:86/:88). Additive — every existing caller keeps compiling unchanged.

**`db_tests.rs` — new shared helpers** (existing helpers `group_room` :77, `new_participant` :309, `insert_channel_mute` :142, `insert_side_effect_job` :180, `count_notifications` :225, `count_notify_outbox` :244, `count_bundles` :255, `pending_notify_outbox` :279, `side_effect_job_state` :296, `BATCH_SERIAL` :71 reused as-is — no signature changes to anything at1–at7 uses):

```rust
/// Message-scoped notification rows: (participant, kind, actor, delivery_id),
/// ORDER BY id — assert as sets, never order.
async fn notification_rows(pool: &PgPool, message_id: MessageId)
    -> Vec<(ParticipantId, String, ParticipantId, Option<uuid::Uuid>)>;
//   SELECT participant_id, kind, actor_id, delivery_id FROM notifications
//    WHERE message_id = $1 ORDER BY id

/// Pending notify outbox row state for one message: (attempts, last_error,
/// available_at, claimed_at, published_at). Message-scoped — no id plumbing.
async fn outbox_row_state(pool: &PgPool, message_id: MessageId)
    -> Option<(i32, Option<String>, Option<time::OffsetDateTime>,
               Option<time::OffsetDateTime>, Option<time::OffsetDateTime>)>;
//   SELECT attempts, last_error, available_at, claimed_at, published_at
//     FROM event_outbox WHERE message_id = $1 AND event_kind = 'notify'
//    ORDER BY aggregate_version LIMIT 1

/// Re-arm a re-parked notify row (mark_failed already cleared claimed_at).
/// Returns rows_affected — callers MUST assert 1 (vacuity guard).
async fn backdate_outbox_row(pool: &PgPool, message_id: MessageId) -> u64;
//   UPDATE event_outbox SET available_at = now() - interval '1 minute'
//    WHERE message_id = $1 AND event_kind = 'notify' AND published_at IS NULL

/// Re-arm a re-parked side-effect job for the 2nd dispatch (AT-3d).
async fn backdate_side_effect_job(pool: &PgPool, job_id: uuid::Uuid) -> u64;
//   UPDATE message_side_effect_jobs SET available_at = now() - interval '1 minute'
//    WHERE id = $1 AND completed_at IS NULL

/// Side-effect job state incl. scheduling fields (AT-3d):
/// (completed_at, attempts, last_error, available_at, claimed_at).
async fn side_effect_job_row(pool: &PgPool, job_id: uuid::Uuid)
    -> (Option<time::OffsetDateTime>, i32, Option<String>,
        Option<time::OffsetDateTime>, Option<time::OffsetDateTime>);

/// Bundle row state for one message (AT-4 precondition):
/// (participant, thread_root, kind, delivery_id).
async fn bundle_row(pool: &PgPool, message_id: MessageId)
    -> Option<(uuid::Uuid, Option<uuid::Uuid>, String, Option<uuid::Uuid>)>;

/// The bundle row ids flush() will consume, read BEFORE flush (AT-4).
async fn bundle_delivery_ids(pool: &PgPool, message_id: MessageId) -> Vec<uuid::Uuid>;
//   SELECT id FROM notification_bundles WHERE message_id = $1 ORDER BY id

/// Recompute the flush outbox event_id in-test: UUIDv5 under the frozen
/// namespace 37301db9-… over "{pid}:{room}:{root}:{sorted ids with colons}"
/// (notification_bundle.rs:450–462). Namespace hardcoded — drift fails loudly.
fn expected_bundle_delivery_id(participant: ParticipantId, room: RoomId,
    thread_root: Option<MessageId>, bundle_ids: &[uuid::Uuid]) -> uuid::Uuid;

/// Pump the global outbox relay until nothing is due (bounded). Runs with the
/// knob DISARMED. Restores the exact-count precondition AT-3b/AT-3c need after
/// earlier tests left pending rows (F1).
async fn drain_pending_outbox(svc: &ImService);
//   for _ in 0..50 { if svc.dispatch_event_outbox_batch(100).await.unwrap() == 0 { return; } }
//   panic!("outbox did not drain to quiescence");
```

**Test placement** (execution order under `--test-threads=1` = declaration order): AT-1a–1e, AT-2, AT-4 → `notifications_tests.rs` (declared after the existing at5g — they seed only message-scoped rows and need no lock); AT-3a–3d → `relay_tests.rs` **declared after at7** (drain makes exactness order-independent anyway; declaration-after keeps the at1–at7 semantics untouched).

---

## §4 Test plan (AT-1a … AT-5)

Common setup: fresh `notification_service(pool, bundles)` per test (fresh `Arc<MockBus>` → fresh knobs, no cross-test leakage), fresh participants via `new_participant` (nil-workspace enrollment), fresh room via `group_room`. All `#[tokio::test] #[ignore = "requires running Postgres with migrations applied"]`.

### AT-1 — user-group `@handle` fan-out (AC-1) — `notifications_tests.rs`

Setup (all sub-tests): `notification_service(pool, false)`; `group_room` (alice sender, bob, carol, dave); eve = `new_participant` (workspace member, **not** room member). `let groups = UserGroupRepo::new(pool.clone());` `let g = groups.create_authorized(WorkspaceId::from_uuid(Uuid::nil()), "team", "Team", alice.id).await.unwrap();` then `add_member_authorized(nil_ws, g.id, X, alice.id)` for bob, carol, eve (alice holds a nil-workspace `Member` role via `new_participant` — `lock_workspace_role` passes; `lock_workspace_members` passes for all three). `groups.resolve(nil_ws, "team")` normalizes "team" (already normalized). Messages use `Block::text("hello @team")` — `group_handle_tokens` (orig.rs:1120) extracts `team`, `is_broadcast_token("team")` is false.

- **AT-1a expansion ∩ room-members, excluding sender**: alice sends `"hello @team"`. `notification_rows(msg.id) == {(bob, "mention", alice, _), (carol, "mention", alice, _)}` (set-compare); `count_notifications(msg.id, None) == 2`; no row for alice (sender exclusion), dave (group non-member), eve (group member but not room member).
- **AT-1b `or_insert` never downgrades**: (i) bob sends plain root (0 rows for root); alice replies `reply_to = Some(root.id)` with `"@team"` → bob's row kind `"reply"` (reply-author branch inserted `Reply` first; group expansion's `or_insert(Mention)` :764 must not downgrade), carol's `"mention"`. (ii) alice sends `[Block::Mention { participant: bob.id }, Block::text("@team")]` → exactly one bob row, kind `"mention"` (direct mention; no duplicate).
- **AT-1c block**: `BlockRepo::block(bob, alice)` (bob blocks sender; `blockers_of(alice)` :818–832) → bob absent, carol present.
- **AT-1d DND**: `NotificationPrefsRepo::set_dnd(bob, Some(0), Some(1440))` (hole-free, V18) → bob absent, carol present.
- **AT-1e room mute**: `insert_channel_mute(bob, room)` (raw SQL — `NotificationPrefsRepo::mute` is `#[cfg(test)] pub(crate)` in aero-storage, unreachable) → bob absent, carol present.

### AT-2 — keyword-alert fan-out (AC-2) — `notifications_tests.rs`

Setup: `notification_service(pool, false)`; `group_room` (alice sender, bob, carol, dave); eve = `new_participant`. `let alerts = KeywordAlertRepo::new(pool.clone());` `add_authorized(bob, nil_ws, "invoice")`, `add_authorized(alice, nil_ws, "invoice")` (sender's own), `add_authorized(eve, nil_ws, "invoice")` (non-room-member), `add_authorized(carol, nil_ws, "payroll")` (non-matching; `validate_keyword` normalizes).

- alice sends `Block::text("send the invoice now")` → `matching_subscribers(nil_ws, text)` returns {bob, alice} (case-insensitive `position('invoice' IN lowered) > 0`; carol's "payroll" doesn't match; eve matches but has no alert) → delivery loop (:787–789 sender exclusion + `member_set.contains`) → `notification_rows == {(bob, "mention", alice, _)}` — **exactly 1 row**; nothing for alice, eve, carol, dave.
- Negative control: alice sends `"nothing relevant here"` → 0 rows for that message.

### AT-3 — relay failure injection (AC-3) — `relay_tests.rs`, after at7

All rows seeded via `send_message` with mentions on `bundles=false` services (Notify outbox row left pending by the fast path — F1a). Knobs armed **after** seeding; assertions use `outbox_row_state` / `side_effect_job_row` / `bus.published` snapshots.

- **AT-3a re-park with backoff** (no lock — all publishes fail, counts exact regardless of leftovers): `drain_pending_outbox`; seed one mention message; snapshot `bus.published.len()`; `arm_failures(usize::MAX)`; `dispatch_event_outbox_batch(10)` == `Ok(0)`. State: `attempts == 1` (claim-time increment, V14), `last_error` contains `"injected failure"`, `claimed_at IS NULL`, `published_at IS NULL`, `available_at ∈ (now, now + 5s)` (`outbox_backoff_delay(1) = 1s`). Bus delta **0** (F2). Then `backdate_outbox_row(msg.id)` (assert rows_affected == 1); dispatch again == `Ok(0)`; `attempts == 2`, `available_at` re-backed-off (≈2s window), `last_error` refreshed.
- **AT-3b partial-batch published count**: `drain_pending_outbox`; seed **two** mention messages (msg1, msg2); snapshot bus len; `arm_failures(1)`; `dispatch_event_outbox_batch(10)` == `Ok(1)` (drain guarantees the pending set is exactly our two rows — F1; which row fails first is deterministic per run but not asserted — F8). Exactly one of the two rows: `published_at IS NOT NULL`; the other: `attempts == 1`, `last_error` set, `available_at` future, `published_at IS NULL`. Bus delta == 1; the single new frame's `event_id == notify_delivery_id(published_msg.id, NotifyBatchKind::Mention)` (deterministic — orig.rs:89; identify the published row from state, then compare).
- **AT-3c superseded lease (`mark_failed` → `Ok(false)`) fenced without panic**: `drain_pending_outbox`; seed one mention message; `arm_gate(entered_tx, release_rx)`; spawn `dispatch_event_outbox_batch(10)`; await `entered` (the blocked publish is **ours** — drain); read `outbox_row_state`: `attempts == 1`, `claimed_at IS NOT NULL`; store `available_at`; simulate the concurrent winner: `UPDATE event_outbox SET published_at = now() WHERE message_id = $1 AND event_kind = 'notify'` (assert rows_affected == 1); drop `release_tx` (or send) → publish returns the injected error → `mark_failed` hits `WHERE published_at IS NULL` → 0 rows → `Ok(false)` arm (debug "superseded lease", outbox.rs:87–95). Join → no panic, `Ok(0)`. Assert fencing did **not** clobber the winner: `published_at IS NOT NULL`, `last_error IS NULL`, `attempts == 1`, `available_at` unchanged from the stored value.
- **AT-3d side-effect error re-park** (covers side_effects.rs:48–93): under `BATCH_SERIAL` (F4). `let ghost = MessageId::new();` (never inserted — no FK, V10); `insert_side_effect_job(pool, ghost, 1, "notifications")`; `dispatch_message_side_effect_batch(10)` == `Ok(0)` (completed count). `side_effect_job_row`: `completed_at IS NULL`, `attempts == 1`, `last_error` contains `"load side-effect message"` (side_effects.rs:105–108 `messages.get(...).context(...)`), `claimed_at IS NULL`, `available_at ∈ (now, now + 5s)` (`side_effect_backoff(1) = 1s`). `backdate_side_effect_job(job_id)` (assert 1); second dispatch == `Ok(0)`; `attempts == 2`, re-parked again.

### AT-4 — `flush_notification_bundles` end-to-end + idempotence (AC-4) — `notifications_tests.rs`

Setup: `notification_service(pool, true)`; `group_room` (alice, bob). alice sends root (plain → 0 rows); bob replies `reply_to = Some(root.id)`.

Precondition (before flush, mirrors at3's bundled leg): `bundle_row(reply.id) == Some((alice, Some(root.id), "reply", Some(notify_delivery_id(reply.id, NotifyBatchKind::Reply))))` — the bundle row's `delivery_id` column is the **notify** id (C2); `count_notifications(reply.id, None) == 0`; `count_notify_outbox(reply.id) == 0` (at3's routing — only flush writes the outbox row); the reply's `notifications`-kind job: `completed_at IS NOT NULL`, `last_error IS NULL` (claim completed at bundle-insert time, V16).

Act: `bundle_ids = bundle_delivery_ids(pool, reply.id)` (non-empty, assert); `expected = expected_bundle_delivery_id(alice.id, room, Some(root.id), &bundle_ids)`; `backdate_bundle(pool, reply.id)` (deadline ≥10s — never env-set, V12); `svc.flush_notification_bundles().await`.

Assert (first flush): `count_bundles(reply.id) == 0` (consumed); `count_notifications(reply.id, Some(alice.id)) == 1` and `notification_rows(reply.id)` == `{(alice, "reply", bob, Some(expected))}` (the **bundle** delivery id, distinct from the bundle row's `notify` id — C2/F6); `count_notify_outbox(reply.id) == 1`; `pending_notify_outbox(reply.id)` → `event_id == expected` (pending: `published_at IS NULL`, awaiting the relay); payload decodes to `RoomEvent::NotifyBatch { room_id, message_id: reply.id, by: bob, delivery_id: expected, recipients: [NotifyTarget { participant: alice, kind: Reply }] }`; the job remains completed.

Assert (idempotent second flush): `svc.flush_notification_bundles().await` again → `count_bundles == 0`, `count_notifications == 1`, `count_notify_outbox == 1` (flush selects zero expired bundles; nothing inserted).

### AT-5 — acceptance gates (AC-5)

`cargo check --workspace` clean · `cargo clippy --workspace --all-targets` no new warnings · `cargo test --workspace --lib` green (new tests `#[ignore]`-gated) · `scripts/truth-check.sh` 0 violations (all new helpers reachable from `#[tokio::test]` fns) · `scripts/file-size-check.sh` under thresholds.

---

## §5 Compatibility constraints

1. **AGENTS.md §4.2 hard rules**: zero new clippy warnings (`db_tests.rs` carries `#![allow(clippy::unwrap_used)]`; submodules inherit), no root-file changes, no new deps, no migrations, `#[ignore]`-gating keeps the hermetic `--lib` run green. Tests registered in `db_tests.rs` submodules exactly like the existing suite.
2. **Zero production-code touch**: `dispatch_notifications`, `notify_delivery_id`, both relays, `flush_notification_bundles`, all repos byte-identical. `notification_service()` gains **additive** builders only.
3. **Existing at1–at7 suite reused as-is**: no changes to its tests or helpers; new helpers are additive; `side_effect_job_state`/`pending_notify_outbox`/`count_*` signatures untouched (AT-3d uses the new `side_effect_job_row` 5-tuple instead of widening the 3-tuple).
4. **MockBus additive fields**: `#[derive(Default)]` still compiles (Atomics/Mutex default); existing `published` assertions unchanged; `subscribe` remains `unimplemented!` — no test may call any subscribing API (relay paths only publish).
5. **Knob-arming discipline**: knobs armed only after seeding (send fast-path publishes must record); each test constructs its own `MockBus` via `notification_service` so no knob leaks across tests.
6. **`--test-threads=1` + `BATCH_SERIAL`** (runbook belt-and-braces, prior design §5.6): AT-3d holds `BATCH_SERIAL` (globally-claimable due side-effect job); AT-3a–3c rely on drain-to-quiescence + the runbook serialization for exact batch counts (outbox side, no lock — at6a's established pattern).
7. **Timing-tolerant assertions**: backoff windows asserted as ranges `(now, now + 5s)`; DND `(0, 1440)`; no sleeps — every re-dispatch is driven by explicit `backdate_*` SQL.
8. **Flush deadline is construction-time env** (`AERO_BUNDLE_DEADLINE_SECS`): never set in-test (process-global); backdate `created_at` via SQL (V12).
9. **Set-compare discipline**: `notification_rows` asserted as sets (production fan-out order is `BTreeMap` participant order; SQL order is `id`).

---

## §6 Failure modes

### Test-infrastructure failure modes (each designed out)

| Mode | Trigger | Mitigation |
|---|---|---|
| Exact-count pollution | earlier tests' pending Notify rows claimed by AT-3's batch (F1) | `drain_pending_outbox` before seeding AT-3a/3b/3c; at6b's leftover row is the concrete case (drain publishes it harmlessly) |
| Gate deadlock | test panics while `publish` is blocked | two-oneshot gate: release sender drop → `RecvError` → injected error → task completes (F5); join wrapped in `tokio::time::timeout` |
| Gate blocks the wrong row | leftover row claimed before ours | drain precondition → our row is the only pending row (F1) |
| MockBus absolute-count drift | seeding records Message frames | snapshot-delta assertions (F2) |
| Vacuous pass | backdate/seed matches 0 rows | every `UPDATE`-style helper returns rows_affected, asserted == 1; bundle/outbox preconditions asserted before the act; drain panics on non-quiescence |
| DND boundary flake | `(0, 1439)` at 23:59 UTC | `(0, 1440)` hole-free (V18) |
| Backoff timing flake | exact `available_at` equality | range `(now, now + 5s)` (V13) |
| Parallel interference | shared tables / global claimers | per-test fresh rows; message-scoped counts; `BATCH_SERIAL` (AT-3d); `--test-threads=1` in the runbook |
| Parallel flush/env | `AERO_BUNDLE_DEADLINE_SECS` set in-test | never set; SQL backdate (V12) |

### Product regressions these tests guard (the reason for the suite)

1. **Group/keyword fan-out silently over- or under-delivers** — wrong intersection, sender included, non-member notified, stronger kind downgraded (`or_insert`), suppression layer bypassed for group/keyword-expanded targets (AT-1a–1e, AT-2).
2. **Relay failure strands rows** — `mark_failed` never re-parks (`available_at` stays past → invisible to `claim_due` → notification lost forever) (AT-3a/3d).
3. **Superseded-lease clobber** — a stale failure overwriting a concurrent winner's completion → double-publish/duplicate notification (AT-3c).
4. **Partial-batch accounting regression** — `dispatch_event_outbox_batch` miscounting successes after a mid-batch failure (AT-3b).
5. **Flush loses deferred replies** — bundle consumed without inbox/outbox materialization, or duplicated on re-flush (idempotence broken), or the deterministic `bundle_delivery_id` contract drifts (AT-4).
6. **Claim-completion timing regression** — side-effect claim completing at flush time instead of bundle-insert time would strand replayed jobs (AT-4 precondition).

---

## §7 Migration / deployment steps — runbook

**No schema or production migration.** The "migration" is the DB-gated acceptance gate per AGENTS.md §4.3. The existing `scripts/test-notification-fanout.sh` already implements the full procedure for the at1–at7 suite; the new tests live in the same `db_tests::` modules so **no script change is required** (verified: the script runs `cargo test -p aero-im-core --lib --locked db_tests:: -- --ignored --test-threads=1`, which selects the new tests too).

```bash
bash scripts/test-notification-fanout.sh
# = throwaway DB aero_test_notif_$$ (guarded name, never pre-dropped, EXIT/INT/TERM trap)
#   build BEFORE migrate (migrations are compile-embedded — AGENTS.md §4.2)
#   migrate via `cargo run --locked --bin aero-cli -- migrate` with DATABASE_URL + AERO__DATABASE__URL
#   hermetic gate: cargo test --workspace --lib --locked
#   suite:         cargo test -p aero-im-core --lib --locked db_tests:: -- --ignored --test-threads=1
#   standing gates: cargo check --workspace --all-targets --locked && clippy && truth-check && file-size-check
#   dropdb --force aero_test_notif_$$
```

Manual equivalent: the runbook in req spec §6 (build → `createdb aero_test_relay` → both URL exports → migrate → `--ignored --test-threads=1` → gates → `dropdb`). Migration dependencies exercised (all pre-existing): 0001, 0002, 0018, 0137 (`notifications.delivery_id`), 0143, 0157, 0162/0163 (`event_outbox` + `event_kind`), 0165 (`message_side_effect_jobs`, `notification_bundles.delivery_id`), 0174. Clean-env precondition (`AERO_BLOCKED_WORDS`/`AERO_PII_GUARD*`/`AERO_AI_*` unset) enforced by the script — unchanged.

---

## §8 Testable acceptance mapping

| # | Criterion (evidence AC) | Deterministic assertion | Fails when |
|---|---|---|---|
| AC-1 | `notification_service()` gains `with_user_groups` + `with_keyword_alerts`; group mention → members ∩ room-members minus sender, no kind downgrade, block/mute/DND filtered | AT-1a–1e: `notification_rows` sets + kind strings; 2 rows / 0 rows per suppression leg | fan-out intersection, self-exclusion, `or_insert`, suppression, or wiring (REQ-0) regressed |
| AC-2 | keyword-alert subscriber gets `Mention` only when room member and not sender; non-matching get nothing | AT-2: exactly `{(bob,"mention",alice)}` for matching text; 0 rows for non-matching text | `matching_subscribers` scope, sender exclusion, non-member leak |
| AC-3 | failure injection: re-park with `last_error`/`attempts+1`/backoff; correct partial-batch count; superseded `Ok(false)` fenced without panic | AT-3a: `Ok(0)` + state tuple + bus delta 0, twice; AT-3b: `Ok(1)` + exactly one published + one re-parked + 1 frame with deterministic `event_id`; AT-3c: `Ok(0)` no panic + winner intact (`published_at` set, `last_error` NULL); AT-3d: `Ok(0)` + job re-parked with "load side-effect message" | re-park lost, lease clobber, miscount, panic, error string drift |
| AC-4 | flush: bundle consumed, inbox row inserted, Notify outbox pending with deterministic id, claim completed; second flush idempotent | AT-4: preconditions (bundle row, 0/0 counts, job completed) → flush → 0 bundles / 1 notification with `bundle_delivery_id` / 1 pending outbox with same id + decoded `NotifyBatch` / job still completed → second flush no-op | flush loss/duplication, id contract drift, claim-timing regression |
| AC-5 | full suite green | AT-5: check / clippy `--all-targets` / hermetic `--lib` / truth-check / file-size-check | any new warning, hermetic break, dead code |

---

## §9 Open items / risks

1. **Side-effect superseded-lease arm (`mark_failed` → `Ok(false)` on `message_side_effect_jobs`) is not deterministically injectable** without a production seam (the outbox arm is covered by AT-3c; the side-effect arm's WHERE additionally requires `completed_at IS NULL`). Accepted gap, per req spec §3 non-goal 4.
2. **AT-3b's "which row fails first" is deterministic per run but unasserted** — the state partition is the contract; if a future reordering of `claim_due` changes priority, the test still passes (by design).
3. **`dispatch_event_outbox_batch(100)` drain publishes other tests' leftover rows through the shared MockBus** — harmless (recorded before the snapshot; at1–at7 assert message-scoped state only), but a future test that asserts absolute `bus.published.len()` would break — documented convention: snapshot deltas.
4. **Gate-on-panic safety relies on oneshot drop semantics** — if a future edit replaces the gate with a barrier/notify, a panicking test could hang the suite; keep the two-oneshot shape.
5. **`AERO_BUNDLE_DEADLINE_SECS` in the test environment** would still be honored (construction-time read) — the tests backdate `created_at` so they pass under any deadline ≥ 0; the floor-10s default needs the 1-minute backdate, which the helper provides.

---

## §10 Decision ledger (design-time corrections to the spec, all evidence-backed)

| # | Spec text | Design correction | Anchor |
|---|---|---|---|
| D1 | AT-3b "seed two pending Notify rows; dispatch → Ok(1)" | Precondition `drain_pending_outbox` (at6b leaves one pending row; the injected failure would otherwise hit it first → `Ok(2)`) | F1, claim order `event_outbox.rs:330–332` |
| D2 | AT-3a "MockBus recorded zero publishes" | Snapshot-delta assertion (seeding records Message frames) | F2 |
| D3 | AT-3c "row is claimed: attempts==1, claimed_at set" | `outbox_row_state` 5-tuple includes `claimed_at`; winner simulated by message-scoped UPDATE + rows_affected==1 | F1/F4 |
| D4 | AT-3d "under BATCH_SERIAL" | Explicitly holds `BATCH_SERIAL` (globally-claimable due job — same class as AT-2/6b/7) | F4, db_tests.rs:71–75 |
| D5 | AT-1 kind/actor assertions | New message-scoped `notification_rows` helper — `NotificationRepo::list` is participant-scoped (unusable) | F3, notification.rs:304 |
| D6 | MockBus knob "exact field shape is implementer's choice" | `AtomicUsize` + two-oneshot gate, panic-safe by drop semantics | F5, tokio dev-deps `sync` |
| D7 | AT-4 payload decode | Exact shape `NotifyBatch { by: actor, recipients: [NotifyTarget { participant, kind }] }` | F6, notification_bundle.rs:346–356 |
