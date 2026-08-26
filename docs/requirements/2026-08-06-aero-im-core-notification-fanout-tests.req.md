# Requirements Spec — DB-gated integration tests for the notification fan-out pipeline

- **Module**: `crates/aero-im-core`
- **Source direction**: `docs/auto/analyses/crates-aero-im-core-cfe64e80.json` (direction 1)
- **Date**: 2026-08-06 · **Status**: source implementation complete; DB-gated acceptance remains
- **Current source (2026-08-19)**: `db_tests/notifications_tests.rs` and
  `db_tests/relay_tests.rs` cover the targeting/suppression, idempotent
  bundle/outbox, relay failure fencing, and stale side-effect cases described
  here. The suite stays `#[ignore]` because it requires a throwaway migrated
  Postgres (and Redis for the presence narrowing branch).
- **Scope**: `#[ignore]`-gated Postgres integration tests only. Zero production-code changes. Tests cover `ImService::dispatch_notifications` (all 10 suppression/targeting layers listed in the acceptance) and the two relay loops `dispatch_event_outbox_batch` / `dispatch_message_side_effect_batch`, driven through real repositories against a real Postgres.

---

## 1. Evidence verification (every direction citation re-checked against `master`)

| Direction claim | Verified finding |
|---|---|
| `dispatch_notifications` is the largest untested logic in the crate (~300 lines, 10+ suppression layers) | ✅ `crates/aero-im-core/src/service/orig.rs:652` (`pub(crate) async fn dispatch_notifications`, spans 652–999 ≈ 350 lines). Layers present in order: reply-to-parent author (676–683), `mentioned_participants` (685–692), thread subscribers (694–713), broadcast tokens `@channel/@everyone/@all` vs `@here` (730–758), user groups (771–792), keyword alerts (794–812), block filter (818–832), workspace mute (834–867), thread mute (869–885), per-thread level (887–925), batch mute/DND/snooze filter (927–965), mention-vs-reply partition (967–974), immediate `insert_many_outboxed` (976–993), bundle `insert_many_idempotent` (995–1016) or legacy immediate reply insert (1018–1034), empty-target claim completion (1035–1038) |
| `dispatch_notifications` appears in no test file | ✅ `rg "dispatch_notifications"` — only `orig.rs:652` and `side_effects.rs` (call site). `orig/tests.rs`, `db_tests.rs`, `db_tests/*` contain zero references |
| `orig/tests.rs` covers only pure helpers | ✅ `orig/tests.rs` (311 lines): `notify_delivery_id_is_deterministic_per_message_and_kind:15`, `mentioned_participants_dedups_in_order_and_ignores_non_mentions:53`, `group_handle_tokens_extracts_lowercases_and_dedups:72`, broadcast-token tests, `can_join_public_channel`, `post_allowed`, `env_truthy` — all pure functions, no DB |
| `db_tests.rs` exercises send/PII/access/edit/recall only; "notification" appears solely in a comment about calls | ✅ `db_tests.rs` (758 lines) + submodules `auto_mod_tests.rs`/`recall_tests.rs`/`room_kind_tests.rs`. The word "notification" appears exactly once: `db_tests.rs:505` — a comment about live-call ring. Harness: `#[ignore = "requires running Postgres with migrations applied"]`, `pool()` from `DATABASE_URL`, `service()` wiring `with_workspaces` + `with_block_repo`, `new_participant()` enrolling into the nil (default) workspace |
| Relay loop `dispatch_event_outbox_batch` (claim/publish/mark_published) has no integration tests | ✅ `service/outbox.rs:27` (`dispatch_event_outbox_batch`), `:57` `finish_claimed_outbox`, `:104` `publish_claimed_outbox` (materialize → `filter_notify_recipients` :283 → seq assign → `bus.publish_bytes_idempotent` → `mark_published` :447). Tests at `outbox.rs:340+` are pure-function only (`materialize_outbox_payload`, `filter_notify_recipients`, `outbox_wire_bytes`) |
| Relay loop `dispatch_message_side_effect_batch` (claim/complete/fail loop) has no integration tests | ✅ `service/side_effects.rs:15` (`dispatch_message_side_effect_batch`), `:19` `dispatch_message_side_effects_for`, `:48` `finish_message_side_effect` (complete vs `mark_failed` re-park), `:102` `process_message_side_effect`. Sole test: `lease_is_longer_than_the_short_poll_interval` (`side_effects.rs:160`) |
| `insert_many_outboxed` with `delivery_id` ON CONFLICT dedup | ✅ `aero-storage/src/notification.rs:173` `insert_many_outboxed` — `INSERT … ON CONFLICT (delivery_id, participant_id) WHERE delivery_id IS NOT NULL DO NOTHING` (:255–256), outbox row inserted with `event_id = delivery_id` (:263–283 → `event_outbox.rs:193` `insert_idempotent_in_tx`, `ON CONFLICT (event_id) DO UPDATE`), claim completed atomically (:285–290). Partial unique index from `migrations/0137_notify_idempotent.sql`; a storage-level (non-`ImService`) dedup test exists at `notification.rs:616` |
| `orig.rs` has zero `metrics::` calls (fail-open = warn-only) | ✅ `rg "metrics" orig.rs` → 2 matches, both in a doc comment and the `per_tenant_metrics_enabled` helper name — no metric increments anywhere in the notification pipeline. (Fixing observability is a *separate* selected direction; not in scope here) |

**Discrepancies found (testability constraints, not fact errors)** — all four shape how the tests must be written:

1. **`message_side_effect_jobs` dedups `ON CONFLICT (message_id, mutation_version, kind) DO NOTHING`** (`aero-storage/src/message_side_effect.rs:111–131`). A redelivery therefore re-claims the **same row** (lease expiry → higher `attempts`), it does *not* insert a second row. Simulating redelivery = re-arm the row (`UPDATE … SET completed_at = NULL, claimed_at = NULL, attempts = attempts + 1, available_at = now() - interval '1 minute'`), **not** re-INSERT.
2. **`NotificationPrefsRepo::mute`/`unmute` are `#[cfg(test)] pub(crate)` in `aero-storage`** (`notification_prefs.rs:168/188`) — compiled out of the crate as a dependency, so aero-im-core tests cannot call them. Room-mute rows must be seeded with raw SQL (`INSERT INTO channel_mutes (participant_id, room_id, created_at) VALUES ($1,$2,now())`; schema `migrations/0018_notification_prefs.sql`).
3. **`MessageSideEffectRepo::insert_in_tx` is `pub(crate)`** (`message_side_effect.rs:111`) — synthetic jobs must be seeded with raw SQL on `message_side_effect_jobs (id, message_id, mutation_version, kind)`.
4. **`PresenceStore` is a concrete Redis wrapper (`fred::RedisClient`), no trait seam** (`aero-storage/src/presence.rs:86`, `ImService::with_presence` at `orig.rs:479` takes the concrete type). The "@here narrows to online members" sub-check requires **live Redis**; aero-im-core already depends on `fred` (workspace dep, `Cargo.toml`) so tests can build a client from `REDIS_URL` (default `redis://localhost:6379`, same convention as `presence.rs:283`). The presence test self-skips when Redis is unreachable so the standard `--ignored` run stays green without Redis.

---

## 2. Problem statement (as verified)

- `dispatch_notifications` (`orig.rs:652`) is the crate's most failure-tolerant surface — every suppression/expansion store is optional, and **every** lookup failure is fail-open with `warn!` only (`should_notify` `orig.rs:559`, `here_recipients` `orig.rs:1020`, batch mute/DND `orig.rs:930–964`, workspace mute `:850`, thread level `:898`). A regression either over-delivers (leaks who's in a room / spam) or silently drops notifications — with no test and no counter to catch it.
- The two durable relay loops (`outbox.rs:27`, `side_effects.rs:15`) are the production paths that turn committed rows into NATS frames and inbox rows; their claim/complete/mark_failed state machines are untested against the real schema.
- Storage-level dedup is already unit-proven (`notification.rs:616`), so the gap is **service-level orchestration**: targeting (mention/reply/broadcast), suppression ordering (block → workspace mute → thread mute → thread level → room mute/DND/snooze), bundle-vs-immediate routing, deterministic `delivery_id` end-to-end, and the relay state machines.

---

## 3. Non-goals (explicitly out of scope)

1. No production-code changes in any crate — tests only (new test modules + test helpers in `db_tests.rs`).
2. No changes to `orig.rs` structure, `dispatch_notifications` semantics, or the two relay loops (the refactor/metrics directions are separate selected directions).
3. **User-group (`@handle`) expansion and keyword-alert fan-out are not asserted** — they appear in the problem narrative but not in the acceptance checks; their stores (`UserGroupRepo`, `KeywordAlertRepo`) are wired in the shared harness only if trivially free. Acceptance governs.
4. No NATS/Redis dependencies for the DB-gated suite except the single presence sub-check (REQ-4c), which self-skips without Redis.
5. No bundle-flush (`flush_notification_bundles`) integration testing beyond what REQ-3 needs to observe rows in `notification_bundles`.

---

## 4. Requirements

### REQ-0 — Test harness (`crates/aero-im-core/src/db_tests.rs` + new submodules)

- Register two new submodules in `db_tests.rs` (pattern of `recall_tests.rs`): `mod notifications_tests;` and `mod relay_tests;` → new files `crates/aero-im-core/src/db_tests/notifications_tests.rs` (AT-1…AT-5) and `crates/aero-im-core/src/db_tests/relay_tests.rs` (AT-6, AT-7).
- New shared helper in `db_tests.rs` (or `notifications_tests.rs`, re-exported):
  `fn notification_service(pool: PgPool, bundles: bool) -> (ImService, Arc<MockBus>)` — builds `ImService::new(…)` (same 8 args as the existing `service()`), then chains the additive builders: `with_workspaces`, `with_notifications`, `with_notification_prefs`, `with_block_repo`, `with_workspace_mutes`, `with_thread_subs`, `with_thread_mutes`, `with_thread_notification_prefs`, and `with_notification_bundles` iff `bundles`. The `Arc<MockBus>` is returned for publish assertions. Tests needing a store *absent* (e.g. "no bundle store") construct the service themselves from the same recipe minus that builder.
- Shared seeding/assertion helpers (raw `sqlx` is available — already a dev-dependency; `db_tests.rs:33` uses `sqlx::postgres::PgPoolOptions`):
  - `insert_channel_mute(pool, participant, room)` — `INSERT INTO channel_mutes …` (constraint D2).
  - `rearm_side_effect_job(pool, job_id)` — the redelivery simulation from constraint D1.
  - `insert_side_effect_job(pool, message_id, mutation_version, kind)` — raw `INSERT INTO message_side_effect_jobs`.
  - count queries: `SELECT count(*) FROM notifications WHERE message_id=$1 [AND participant_id=$2]`, same for `notification_bundles`, `event_outbox WHERE event_kind='Notify'`, `ai_jobs WHERE message_id=$1`; row lookup for `message_side_effect_jobs` (completed_at/attempts/last_error).
- Every test: `#[tokio::test] #[ignore = "requires running Postgres with migrations applied"]`. Unique participant emails via the existing `unique_email`/`new_participant` helpers. Fresh room + members per test (existing `svc.create_room` + `add_member`; legacy rooms land in the nil workspace, matching `new_participant` enrollment and the workspace-mute test).

### AT-1 — @mention → inbox rows + NotifyBatch outbox event with deterministic `delivery_id`

Scenario: alice (sender), bob, carol in a group room; `bundles=false` service.
Act: `svc.send_message(alice.id, room, [Block::text("hi"), Block::Mention { participant: bob.id }, Block::Mention { participant: carol.id }], None, None)`.
Assert:
- `notifications` has exactly 2 rows for `msg.id`: (bob, kind `mention`, actor alice) and (carol, kind `mention`, actor alice) — via `NotificationRepo::list` (`notification.rs:304`) or count SQL; **no** row for alice (self-exclusion) and none for a non-mentioned member.
- `event_outbox` has exactly 1 row with `event_kind = 'Notify'` for `msg.id`, and its `event_id` equals `notify_delivery_id(msg.id, NotifyBatchKind::Mention)` (both `pub(crate)` in-crate: `orig.rs:89`, `orig.rs:44`) — the deterministic-id contract.
- The row's `payload` decodes to `RoomEvent::NotifyBatch { room_id, message_id: msg.id, by: alice, delivery_id: <same uuid>, recipients: [bob, carol] }`.
- The send fast path leaves this Notify row **pending** (`EventOutboxRepo::pending_for_message(msg.id)` returns the Notify row; the `Message` row was fast-published at `messages.rs:208`) — this is the precondition AT-6 builds on.

### AT-2 — Redelivery with the same `delivery_id` dedups (no duplicate rows)

Same setup as AT-1; after the fast path completes the original Notifications job (it does — `messages.rs:554` `kick_message_side_effects`), **re-arm** that job row (`rearm_side_effect_job`, constraint D1 — the crash-before-receipt simulation) and run `svc.dispatch_message_side_effects_for(msg.id)`.
Assert:
- Row counts unchanged: `notifications` for `msg.id` still 2 total (1 per participant); `event_outbox` `Notify` rows for `msg.id` still 1 (the second `insert_idempotent_in_tx` collapses on `ON CONFLICT (event_id)`).
- The re-armed job is `completed_at IS NOT NULL`, `last_error IS NULL` — the redelivery was processed to completion, not failed.

### AT-3 — Reply notifications: bundles when wired, immediate otherwise

Setup: alice sends root; bob replies (`reply_to = Some(root.id)`); target = root author alice (`orig.rs:676–683`).
- **Immediate** (`bundles=false`): 1 `notifications` row (alice, kind `reply`, message = reply.id); 1 `event_outbox` `Notify` row with `event_id == notify_delivery_id(reply.id, NotifyBatchKind::Reply)`; 0 `notification_bundles` rows.
- **Bundled** (`bundles=true`): 0 `notifications` rows; exactly 1 `notification_bundles` row with `participant_id = alice`, `message_id = reply.id`, `thread_root = root.id`, `kind = 'reply'`, `delivery_id == notify_delivery_id(reply.id, NotifyBatchKind::Reply)` (schema `notification_bundle.rs:39`, insert `:89`); the side-effect job completed (bundle insert carries `Some(source)`).
- Mixed message (mention + reply) with bundles wired: mention rows land in `notifications` (claim completed by the bundle insert, `orig.rs:981` `completion = (!has_replies).then_some(source)`), reply row in `notification_bundles`.

### AT-4 — `@everyone` vs `@here` target sets

Members: alice (sender), bob, carol, dave. Message text `"@everyone"` / `"@here"` (parsed by `group_handle_tokens` `orig.rs:1120`; `is_all_broadcast_token` = `channel|everyone|all`, `is_here_token` = `here`).
- **AT-4a `@everyone`, no presence wired**: `notifications` rows for exactly {bob, carol, dave}, kind `mention`, none for alice.
- **AT-4b `@here`, no presence store** (`bundles=false` service without `with_presence`): same full set — the documented legacy fail-open (`orig.rs:1025` `let Some(presence) = self.presence.as_ref() else { return member_set.clone() }`).
- **AT-4c `@here`, presence store wired** (Redis-gated; self-skips if `REDIS_URL` unreachable — constraint D4): stamp `PresenceStore::join(room, bob)` only. Assert `notifications` rows = exactly {bob} (online ∩ members, sender excluded, `orig.rs:1020`). Two additional fail-open branches with Redis up: empty roster (no stamps) → all members; roster disjoint from members (join dave to a *different* room… or stamp an outsider id) → all members (the `intersected.is_empty()` fallback at `orig.rs:1041–1045`).

### AT-5 — Each suppression layer removes exactly the expected recipient

One test per layer; baseline = `@everyone` message (or reply setup below) that would notify {bob, carol, dave} absent the layer; exactly one of them is suppressed; assert the other two rows exist and the suppressed one's row does not. (`bundles=false` everywhere so replies land in `notifications`.)

- **AT-5a room mute**: `insert_channel_mute(bob, room)` → bob absent. (Raw SQL per constraint D2.)
- **AT-5b DND**: `NotificationPrefsRepo::set_dnd(bob, Some(0), Some(1439))` — the full-day window deterministically contains the current UTC minute (`notification_prefs.rs:45` `in_dnd_window`, start-inclusive/end-exclusive, unit-tested) → bob absent.
- **AT-5c snooze**: `set_snooze(bob, Some(now_utc + 10 min))` → bob absent (`is_snoozed` `notification_prefs.rs:71`; 10-min margin avoids boundary flake).
- **AT-5d block**: `BlockRepo::block(bob, alice)` (bob blocks the *sender*) → bob removed by `blockers_of(sender)` (`orig.rs:818–832`).
- **AT-5e workspace mute**: `WorkspaceMuteRepo::mute(bob, WorkspaceId::from_uuid(Uuid::nil()))` (room is a legacy room in the nil workspace — `room.rs:60–64`) → bob absent via the batch `muted_participants` step (`orig.rs:834–867`).
- **AT-5f thread mute**: root by alice; carol + dave `subscribe_authorized` to root (`thread_subscription.rs:193`); bob replies to root → reply targets {alice (author), carol, dave} (`orig.rs:676–683` + thread-subscribers `:694–713`). `ThreadMuteRepo::mute_authorized(carol, root)` → carol absent, alice + dave present.
- **AT-5g thread level `none`** (same reply setup): `set_level_authorized(dave, root, "none")` → dave absent; `set_level_authorized(carol, root, "mentions")` + a reply **without** mentioning carol → carol absent, alice present (`levels_for` default `"all"` for alice — `orig.rs:900`). Positive control: a reply that **mentions** carol → carol present, dave still absent (`"none"` beats the mention).

### AT-6 — Relay loops: outbox rows marked published; side-effect claims completed

- **AT-6a `dispatch_event_outbox_batch`**: from AT-1's state (Notify row pending), call `svc.dispatch_event_outbox_batch(10)` → returns ≥ 1. Assert: `pending_for_message(msg.id)` is `None` (row `published_at` set — `event_outbox.rs:447`); `MockBus.published` contains exactly one frame on `im.room.{room}` for this delivery whose JSON has `event_id == delivery_id` and a numeric `seq` stamp (`outbox_wire_bytes` `outbox.rs:316`, `stamp_seq`); payload decodes to `NotifyBatch` with the same recipients.
- **AT-6b `dispatch_message_side_effect_batch`**: seed a due job (raw insert, `mutation_version = msg.version + 1`, kind `notifications` — a distinct `(message, version, kind)` key per constraint D1), call `dispatch_message_side_effect_batch(10)` → returns 1; the job row is completed; the notification rows for that message appeared. Second call returns 0 (nothing due). Also: a failed job re-parks — make the job's message `deleted_at` non-null… (optional negative control; the deleted/expired branch completes the claim without rows — assert no crash, claim completed, no inbox rows).

### AT-7 — Stale side-effect version completes without spending (version guard)

Scenario (the realistic crash-recovery shape): send message v1 via the `bundles=false` service (fast path completes the v1 Embed/Moderate jobs, enqueueing 1 `ai_jobs` row each). **Edit** the message (`svc.edit_message`, v1 → v2) — the edit transaction appends fresh Embed + Moderate jobs at version 2 (`aero-storage/src/message/events.rs:159–166`) and enqueues nothing itself. Re-arm the **original v1 Embed job** (`rearm_side_effect_job`) so a stale `mutation_version = 1` job is pending.
Act: `svc.dispatch_message_side_effect_batch(10)`.
Assert:
- The stale v1 job row is `completed_at IS NOT NULL` with `last_error IS NULL` — completed, not failed.
- `ai_jobs` count for the message increases by exactly **2** (the two fresh v2 jobs), not 3 — the stale job spent nothing (`side_effects.rs` version-guard branch at `:140`, `message.version != job.mutation_version`).
- Positive control that the guard didn't over-fire: the fresh v2 jobs still enqueued normally.

---

## 5. Constraints & risks

- **AGENTS.md §4.2**: no new clippy warnings (`cargo clippy --workspace --all-targets`), no new root files, `#[ignore]`-gated tests keep `cargo test --workspace --lib` hermetic, tests live under `db_tests/` registered in `db_tests.rs`, and the DB-gated suite must run against a fresh throwaway DB (`CREATE DATABASE` + migrate + `DROP DATABASE`, never the shared dev DB).
- `dispatch_notifications`, `notify_delivery_id`, `NotifyBatchKind`, `should_notify` are `pub(crate)` — directly callable from in-crate tests if a test prefers direct invocation over the relay path; the spec drives everything through the public relay/service APIs (`send_message`, `dispatch_message_side_effect_batch`, `dispatch_event_outbox_batch`, `dispatch_message_side_effects_for`) so the tests exercise the production orchestration, with raw SQL only for *seeding* state the public API cannot create (mutes, synthetic/re-armed jobs).
- Deterministic-id assertions depend on `notify_delivery_id` (v5, frozen namespace `orig.rs:87`) — no clock/randomness.
- DND determinism via full-day window; snooze via 10-minute margin — both documented above.
- The Redis-gated AT-4c self-skips on unreachable Redis, so `DATABASE_URL=… cargo test -p aero-im-core --lib -- --ignored` passes with Postgres only; with Redis up it additionally verifies narrowing + both fail-open roster branches.

---

## 6. Acceptance procedure

```bash
# throwaway DB (per AGENTS.md §4.3):
#   createdb aero_test_<run>  →  DATABASE_URL=postgres://…/aero_test_<run>  →  aero-cli migrate  (after cargo build)
DATABASE_URL=postgres://aero:aero@localhost/aero_test_<run> \
  cargo test -p aero-im-core --lib -- --ignored
```

Expected: all 7 acceptance checks green —
1. AT-1 mention → 2 inbox rows (mention kind, actor = sender) + 1 `Notify` outbox row with `event_id == notify_delivery_id(msg, Mention)` and matching payload;
2. AT-2 re-arm + redispatch → row counts unchanged (2 notifications, 1 outbox `Notify`), job completed;
3. AT-3 reply → `notification_bundles` row (with `thread_root` + reply `delivery_id`) when wired, `notifications` reply row + `Notify` outbox when not;
4. AT-4 `@everyone` = all members minus sender; `@here` without presence = same; `@here` with presence (Redis) = exactly the online subset, with empty/disjoint-roster fail-open to all;
5. AT-5a–g each layer removes exactly its one expected recipient (and AT-5g's `mentions`-level positive control);
6. AT-6 `dispatch_event_outbox_batch` publishes + marks published (`pending_for_message` → None, MockBus frame with `event_id`/`seq`); `dispatch_message_side_effect_batch` completes seeded claims (return 1 → 0);
7. AT-7 stale-v1 Embed job completes with `last_error IS NULL` and the `ai_jobs` delta equals exactly the fresh-jobs count (2), not 3.

Plus the standing gates: `cargo check --workspace`, `cargo clippy --workspace --all-targets` (no new warnings), `cargo test --workspace --lib` (hermetic, unaffected).
