# Design — DB-gated integration tests for the notification fan-out pipeline

- **Module**: `crates/aero-im-core` (test-only; zero production-code changes in any crate)
- **Source spec**: `docs/requirements/2026-08-06-aero-im-core-notification-fanout-tests.req.md`
- **Date**: 2026-08-06 · **Status**: design (unimplemented)
- **Scope**: `#[ignore]`-gated Postgres integration tests for `ImService::dispatch_notifications` (orig.rs:652) and the two durable relay loops (`dispatch_event_outbox_batch` outbox.rs:27, `dispatch_message_side_effect_batch` side_effects.rs:15), driven through the public service APIs with raw SQL only for seeding state the public API cannot create.

---

## §1 Verification ledger (evidence was untrusted; every claim re-checked against `master`)

| # | Evidence claim | Verdict | Exact anchor |
|---|---|---|---|
| V1 | `dispatch_notifications` at `orig.rs:652`, ~350 lines, pub(crate), in no test file | ✅ | `service/orig.rs:652` `pub(crate) async fn dispatch_notifications`; only refs: call site `side_effects.rs:133` + doc links. `rg` across repo confirms zero test refs |
| V2 | `orig/tests.rs` (311 lines) covers only pure helpers | ✅ | All 24 tests are pure fn tests (`notify_delivery_id_is_deterministic…:15`, `mentioned_participants…:53`, `group_handle_tokens…:72`, broadcast tokens, `can_join_public_channel`, `post_allowed`, `env_truthy`); `mock_bus_records_publish`/`publish_event_helper_through_dyn` are bus-helper tests, still no DB |
| V3 | `db_tests.rs` mentions "notification" once (`:505`, live-call comment) | ✅ | `db_tests.rs:505` — comment only |
| V4 | Both relay loops have no integration tests | ✅ | `outbox.rs:27`; its tests (`outbox.rs:340+`) are pure (`materialize_outbox_payload`, `filter_notify_recipients`, `outbox_wire_bytes`). `side_effects.rs:15`; sole test `lease_is_longer_than_the_short_poll_interval` (`:160`) |
| V5 | `insert_many_outboxed` ON CONFLICT dedup (`notification.rs:173`, mig 0137) | ✅ | `notification.rs:255` `ON CONFLICT (delivery_id, participant_id) WHERE delivery_id IS NOT NULL DO NOTHING`; `migrations/0137_notify_idempotent.sql` exists. Storage-level dedup test at `notification.rs:616` |
| V6 | `orig.rs` has zero `metrics::` calls (fail-open = warn-only) | ✅ | `rg "metrics"` → 2 hits, both doc comment / helper name `per_tenant_metrics_enabled:147` |
| V7 | `message_side_effect_jobs` dedups `ON CONFLICT (message_id, mutation_version, kind) DO NOTHING` → redelivery = re-arm, not re-insert | ✅ | `message_side_effect.rs:122`; `insert_in_tx` `pub(crate)` at `:111` (unreachable from aero-im-core) |
| V8 | `NotificationPrefsRepo::mute`/`unmute` are `#[cfg(test)] pub(crate)` (`:168/188`) | ✅ | Both under `#[cfg(test)]`; unreachable as a dependency → seed `channel_mutes` via raw SQL (schema mig 0018) |
| V9 | `PresenceStore` is concrete fred wrapper, no trait seam; aero-im-core already depends on fred | ✅ | `presence.rs:86`; `with_presence` takes concrete `aero_storage::PresenceStore` (`orig.rs:479`); `fred.workspace = true` in aero-im-core `Cargo.toml` |
| V10 | `notify_delivery_id` v5, frozen namespace | ✅ | `orig.rs:87` `NOTIFY_DELIVERY_NAMESPACE` const, fn at `:89`; `NotifyBatchKind` at `:59` (spec said :44 — drift, non-material) |
| V11 | Edit appends fresh v2 Embed+Moderate jobs; version guard completes stale jobs without spending | ✅ | `message/events.rs` `edit_outboxed_authorized` → `insert_in_tx(tx, id, message.version, [Embed, Moderate])`; guard at `side_effects.rs:139–144` (`message.version != job.mutation_version` → `complete_claim`) |
| V12 | `@here` fail-open branches | ✅ | `here_recipients` `orig.rs:1020`: no store → full set; empty roster → full set; disjoint intersect → full set (`:1041–1045`); Err → warn + full set |
| V13 | `@everyone`/`@here` token parsing | ✅ | `group_handle_tokens` `orig.rs:1120`; `is_all_broadcast_token` `:1104` (channel/everyone/all); `is_here_token` `:1110` |
| V14 | Legacy rooms land in nil workspace | ✅ | `aero-storage/src/room.rs:74` binds `uuid::Uuid::nil()` as workspace |
| V15 | `MockBus` records publishes; `seq` defaulted | ✅ | `test_util.rs` `MockBus { published: Mutex<Vec<(String, Vec<u8>)>> }`; `ImService::new` defaults `seq` to `LocalSeqProvider` (`orig.rs:338`) → AT-6a wire carries a numeric `seq` without `with_seq` |
| V16 | Fast path is deterministic in tests | ✅ | `messages.rs:554` `kick_message_side_effects` spawns, then `#[cfg(test)]` **awaits** the handle → after `send_message`/`edit_message` return, side-effect dispatch has completed |
| V17 | `event_outbox` helpers exist | ✅ | `pending_for_message:271`, `claim_due:295`, `mark_published:447`, `insert_idempotent_in_tx:193` (ON CONFLICT (event_id) DO UPDATE) |

**Material corrections the design must encode (spec bugs, not fact errors):**

1. **D1 — `event_kind` stores lowercase `'notify'`**, not `'Notify'`. `EventOutboxKind::Notify => "notify"` (`event_outbox.rs:44`). Spec's count SQL `WHERE event_kind='Notify'` would match zero rows. All acceptance SQL uses `event_kind = 'notify'`.
2. **D2 — AT-7's literal scenario yields `ai_jobs` delta 0, not 2.** `edit_message` fast-kicks side effects (`messages.rs:354`, awaited under `cfg(test)`), so the fresh v2 Embed/Moderate jobs complete **during the edit** and their `ai_jobs` rows already exist before the batch dispatch. The "delta = 2 not 3" assertion only fires if the test **re-arms all three pending-relevant jobs** (stale v1 Embed + v2 Embed + v2 Moderate) before dispatching: stale v1 spends 0, v2 pair spends 2. See AT-7.
3. **D3 — `set_dnd(Some(0), Some(1439))` has a 1-minute hole at 23:59 UTC.** `in_dnd_window` (`notification_prefs.rs:45`) is start-inclusive/end-exclusive: minute 1439 is outside `[0,1439)`. Use `set_dnd(bob, Some(0), Some(1440))` — `set_dnd` performs no bounds validation (`:276`) and `now < 1440` is true for every minute of the day. Boundary-proof by construction.
4. **D4 — `notification_bundles.delivery_id` exists only via mig 0165** (`ALTER TABLE … ADD COLUMN delivery_id` + partial unique index), not 0143 (base table has no delivery_id). AT-3's bundle-row `delivery_id` assertion is valid but depends on 0165 being applied — guaranteed by full migrate on a throwaway DB.
5. **D5 — dedup is invisible at the service layer.** `insert_many_outboxed` returns `Ok(true)` even when `ON CONFLICT` collapses every row (no row-count check; claim completed in the same tx, `notification.rs:255–290`). AT-2 must assert idempotency via **row counts + job completion state**, never via a return value.

Minor anchor drift (non-material): `NotifyBatchKind` `:59` (spec `:44`), `notify_delivery_id` `:89` (spec `:87`), `notification_bundle.rs` single insert `:29` / `insert_many_idempotent` `:89` (spec `:39/:89`), send fast-publish is `messages.rs:211` region (spec `:208`).

---

## §2 Design overview

Zero production-code changes, zero new dependencies, zero schema changes. Two new test files plus shared helpers:

```
crates/aero-im-core/src/db_tests.rs                  (+2 mod declarations, +9 helpers)
crates/aero-im-core/src/db_tests/notifications_tests.rs   (AT-1 … AT-5)
crates/aero-im-core/src/db_tests/relay_tests.rs           (AT-6, AT-7)
```

Every test drives the **production orchestration path**: `send_message` / `edit_message` (messages.rs:75/:305) → post-commit `kick_message_side_effects` (messages.rs:554) → `dispatch_message_side_effects_for` (side_effects.rs:31) → `dispatch_notifications` (orig.rs:652) → `insert_many_outboxed` / `insert_many_idempotent` → `dispatch_event_outbox_batch` (outbox.rs:27) → `publish_bytes_idempotent` → `MockBus`. Raw SQL is used **only** to seed state the public API cannot create (constraint D1 from the spec: mutes, synthetic/re-armed jobs, a jobless message row) and to assert row counts.

The tests deliberately **do not** call `dispatch_notifications` directly — the in-crate `pub(crate)` visibility would allow it, but the acceptance must exercise the real claim/complete state machine.

---

## §3 API changes

### 3.1 Production API — none

No public, `pub(crate)`, or `pub` signature in any crate changes. No `Cargo.toml` edits (`fred`, `sqlx`, `time`, `uuid` already available). No migrations added.

### 3.2 Test-only surface (new, all `#[cfg(test)]`-reachable)

In `db_tests.rs` (pattern of the existing `recall_tests.rs`):

```rust
mod notifications_tests;
mod relay_tests;

/// Service with the full notification stack wired. `bundles=false` omits
/// NotificationBundleRepo (replies go immediate). Returns the MockBus for
/// publish assertions.
fn notification_service(pool: PgPool, bundles: bool) -> (ImService, Arc<MockBus>);
// ImService::new(8 args, per db_tests.rs:36–45) + with_workspaces + with_notifications
// + with_notification_prefs + with_block_repo + with_workspace_mutes + with_thread_subs
// + with_thread_mutes + with_thread_notification_prefs + (bundles ⇒ with_notification_bundles).

// Raw-SQL seeding (constraints D1/D3/D4 of the spec):
async fn insert_channel_mute(pool: &PgPool, participant: ParticipantId, room: RoomId);
//   INSERT INTO channel_mutes (participant_id, room_id, created_at) VALUES ($1,$2,now())
async fn rearm_side_effect_job(pool: &PgPool, job_id: uuid::Uuid) -> u64;
//   UPDATE message_side_effect_jobs
//      SET completed_at = NULL, claimed_at = NULL, last_error = NULL,
//          attempts = attempts + 1, available_at = now() - interval '1 minute'
//    WHERE id = $1
// Returns rows_affected — callers MUST assert 1 (vacuity guard C3): a re-arm
// matching 0 rows (job already completed by the fast path) would otherwise let
// AT-2/AT-7 pass without exercising the redelivery at all.
async fn insert_side_effect_job(pool: &PgPool, message_id: MessageId,
                                mutation_version: i32, kind: &str) -> uuid::Uuid;
//   INSERT INTO message_side_effect_jobs (id, message_id, mutation_version, kind)
//   VALUES ($1,$2,$3,$4)   -- kind IN ('notifications','embed','moderate')
// Plain INSERT (no ON CONFLICT): a UNIQUE (message_id, mutation_version, kind)
// collision errors loudly instead of silently no-op'ing; returning the new id
// implies the row exists.
async fn insert_message_row(pool: &PgPool, room: RoomId, sender: ParticipantId,
                            blocks: Vec<Block>) -> MessageId;
//   INSERT INTO messages (id, room_id, sender_id, blocks, version)
//   VALUES ($1,$2,$3,$4::jsonb, 1)   -- mig 0157: version NOT NULL DEFAULT 1
// Only for AT-6b: a message created outside ImService has no side-effect jobs,
// so the seeded notifications job is the *only* dispatch driver.

// Assertion helpers:
async fn count_notifications(pool: &PgPool, message_id: MessageId,
                             participant: Option<ParticipantId>) -> i64;
async fn count_notify_outbox(pool: &PgPool, message_id: MessageId) -> i64;
//   WHERE event_kind = 'notify'   ← lowercase, per correction D1
async fn count_bundles(pool: &PgPool, message_id: MessageId) -> i64;
async fn count_ai_jobs(pool: &PgPool, message_id: MessageId) -> i64;
//   WHERE target_id = $1 AND kind IN ('embed','moderate')
// ai_jobs has NO message_id column (mig 0002) — only target_id; filter on
// target_id exactly as the production query does (message_side_effect.rs:538).
// complete_with_ai_job binds target_id = message_id (:277/:297), so the
// message-scoped count is correct.
async fn pending_notify_outbox(pool: &PgPool, message_id: MessageId)
    -> Option<(uuid::Uuid, serde_json::Value)>;   // (event_id, payload)
async fn side_effect_job_state(pool: &PgPool, job_id: uuid::Uuid)
    -> (Option<OffsetDateTime>, i32, Option<String>);  // (completed_at, attempts, last_error)

fn outbox_subject(room: RoomId) -> String;   // format!("im.room.{room}")
```

Redis presence (AT-4c): built inline in `notifications_tests.rs` —

```rust
async fn try_presence_store() -> Option<aero_storage::PresenceStore> {
    // REDIS_URL unset  ⇒ print skip note and return None (AT-4c self-skips).
    // REDIS_URL set    ⇒ MUST connect within a short timeout; failure PANICS
    //                    ("REDIS_URL set but unreachable: …") — a set-but-down
    //                    Redis must fail the suite loudly, never skip (Q2).
    // No localhost default: an unset var must stay distinguishable from a
    // broken one, so a Postgres-only run can never silently become a half-Redis run.
    let Ok(url) = std::env::var("REDIS_URL") else {
        eprintln!("skipping presence sub-check (REDIS_URL unset)");
        return None;
    };
    let client = fred::prelude::RedisClient::new(fred::types::RedisConfig::from_url(&url));
    // connect with a short timeout; Ok ⇒ Some(PresenceStore::new(client)), Err ⇒ panic!
}
```

No production item's visibility changes; the `#[cfg(test)] pub(crate)` seams (V8, `mute`/`unmute`, `insert_in_tx`) stay unreachable and are worked around with raw SQL exactly as the spec prescribes.

---

## §4 Test plan (AT-1 … AT-7)

Common setup per test: fresh participants via `new_participant` (enrolls into nil workspace, `db_tests.rs:56+`), fresh room via `svc.create_room(alice, RoomKind::Group, …)` + `svc.add_member(alice, room, X)` for each member, fresh `notification_service(pool, bundles)` per test. `pool()` = `PgPoolOptions::max_connections(2).connect_lazy(DATABASE_URL)` (existing convention). Every test: `#[tokio::test] #[ignore = "requires running Postgres with migrations applied"]`.

### AT-1 — @mention → inbox rows + Notify outbox with deterministic delivery_id

- Setup: alice, bob, carol in a group room; `bundles=false`.
- Act: `svc.send_message(alice.id, room, vec![Block::text("hi"), Block::Mention { participant: bob.id }, Block::Mention { participant: carol.id }], None, None)` (signature `messages.rs:75`).
- Assert:
  1. `count_notifications(msg.id, None) == 2`; via `NotificationRepo::list` (`notification.rs:304`) verify rows are (bob, `mention`, actor alice) and (carol, `mention`, actor alice); no alice row (self-exclusion, `orig.rs:676–692`); no non-mentioned member row.
  2. `count_notify_outbox(msg.id) == 1` and its `event_id == notify_delivery_id(msg.id, NotifyBatchKind::Mention)` (both `pub(crate)` in-crate — assertible directly).
  3. `pending_notify_outbox` payload decodes (`serde_json::from_value::<RoomEvent>`) to `RoomEvent::NotifyBatch { room_id, message_id: msg.id, by: alice, delivery_id: <same uuid>, recipients: {bob, carol} }`.
  4. ~~The Notify row is still **pending**~~ — **dropped in review (C1)**: asserting pending state here created a cross-test dependency — AT-6a's global `dispatch_event_outbox_batch` could claim and publish this row mid-test under parallel threads. AT-6a now seeds and asserts its own pending precondition (§4 AT-6a), so no test asserts state another test's global outbox batch can mutate.

### AT-2 — Redelivery with same delivery_id dedups

- Setup: as AT-1 (fast path has completed the original `notifications` job — deterministic because `kick_message_side_effects` awaits under `cfg(test)`, V16).
- Act: look up the job (`SELECT id FROM message_side_effect_jobs WHERE message_id=$1 AND kind='notifications'`), `rearm_side_effect_job` — **assert it returned 1 row** (vacuity guard C3) — then `svc.dispatch_message_side_effects_for(msg.id)`. The re-arm → dispatch → assert section runs under the `BATCH_SERIAL` mutex (§5.6): the re-armed job is globally claimable, and AT-6b/AT-7's global batch could otherwise claim it mid-test.
- Assert (correction D5 — count-based, not return-value):
  1. `count_notifications == 2` (still 1 per participant); `count_notify_outbox == 1` (second `insert_idempotent_in_tx` collapses on `ON CONFLICT (event_id)`).
  2. Re-armed job state `(completed_at = Some, last_error = None)` — the redelivery was processed to completion. The claim is completed inside `insert_many_outboxed`'s tx (`notification.rs:285–290`), so no `Error::Conflict("notification side-effect lease was superseded")` surfaces. Failure channel note (Q1): a dropped 0137 index surfaces as a hard 42P10 SQL error → `mark_failed` → `last_error = Some`; the job-state assertion catches it via the **error channel**, not via counts.

### AT-2b — empty-target completion branch (new in review: previously unasserted)

Every plain, unmentioned message in production routes through the no-recipients branch — `orig.rs:1006–1007` (`if !has_mentions && !has_replies { complete_notification_claim(...) }`) plus the empty-recipient no-ops at `notification.rs:123` / `notification_bundle.rs:146`. A regression here (claim never completed → job parked in retry forever; or a spurious empty outbox row) was invisible to the suite.

- Leg 1 (plain text): `svc.send_message(alice.id, room, vec![Block::text("just text")], None, None)` → fast path completes the notifications job (V16).
  - `count_notifications(msg.id, None) == 0`; `count_notify_outbox(msg.id) == 0`; `count_bundles(msg.id) == 0`.
  - notifications job (`side_effect_job_state`): `completed_at = Some`, `last_error = None`.
  - `svc.dispatch_message_side_effects_for(msg.id)` returns **0** (nothing due for this message) — message-scoped, so no `BATCH_SERIAL` needed.
- Leg 2 (filtered-to-empty): `@everyone` where every member blocks the sender (`BlockRepo::block(m, alice)` for bob/carol/dave) → the broadcast expands targets (`orig.rs:729`) then the blockers filter removes them all (`orig.rs:792`) → `has_mentions` false → same assertions: 0 rows, job completed, scoped redispatch returns 0.

### AT-3 — Reply notifications: bundles when wired, immediate otherwise

- Setup: alice sends root; bob replies (`reply_to = Some(root.id)`); target = root author alice (`orig.rs:676–683`).
- Immediate (`bundles=false`): `count_notifications(reply.id) == 1` (alice, kind `reply`); `count_notify_outbox(reply.id) == 1` with `event_id == notify_delivery_id(reply.id, NotifyBatchKind::Reply)`; `count_bundles(reply.id) == 0`.
- Bundled (`bundles=true`): `count_notifications == 0`; exactly 1 `notification_bundles` row with `participant_id = alice`, `message_id = reply.id`, `thread_root = root.id`, `kind = 'reply'`, `delivery_id == notify_delivery_id(reply.id, Reply)` (column exists via mig 0165, correction D4); side-effect job completed (bundle insert carries `Some(source)`, `notification_bundle.rs:89–180`).
- Mixed message (mention + reply) with bundles wired: mention rows land in `notifications`; reply row in `notification_bundles`. Counts: 1 mention notification + 1 bundle + **one** outbox notify row — `insert_many_idempotent` writes **no** outbox row (only `flush()` does, `notification_bundle.rs:198/352`; correction C2). The outbox row's `event_id == notify_delivery_id(msg, Mention)` and the bundle row's `delivery_id == notify_delivery_id(msg, Reply)` — distinct tags (`orig.rs:59–75`) — so the two ids are distinct *by construction* while only one outbox row exists. The claim is completed by the bundle insert (`Some(source)`); the mention insert passed `completion = (!has_replies).then_some(source)` = `None` (`orig.rs:981`).

### AT-4 — `@everyone` vs `@here` target sets

Members alice (sender), bob, carol, dave. Message `[Block::text("@everyone")]` / `[Block::text("@here")]` (tokens from `group_handle_tokens`, `orig.rs:1120`).

- **AT-4a `@everyone`** (no presence wired): rows exactly {bob, carol, dave}, kind `mention`; none for alice.
- **AT-4b `@here`, no presence store**: same full set — `here_recipients` legacy fail-open (`orig.rs:1020` `let Some(presence) = … else { return member_set.clone() }`).
- **AT-4c `@here`, presence wired** (Redis-gated per §3.2): `try_presence_store()` returns `None` **only when `REDIS_URL` is unset** (skip note, test returns); when `REDIS_URL` is set the connect must succeed — a set-but-unreachable Redis **panics the test** (review Q2: the old silent skip masked a dead CI Redis). The Redis leg is a **required** acceptance leg (§7 presence-leg contract + gate script), not an optional extra:
  - `PresenceStore::join(room, bob)` only → rows exactly {bob} (online ∩ members, sender excluded, `orig.rs:1020–1045`).
  - Empty roster (no joins) → all members (`Ok(_)` branch).
  - Disjoint roster (join dave to a **different** room) → all members (`intersected.is_empty()` fallback).
  - Rooms are fresh UUIDs per test, so no cross-run Redis pollution; TTL 45s (`presence.rs:29`) irrelevant within a test.
  - What this does **not** guard (Q2.3): production boot wiring (`with_presence`). The suite builds `ImService` itself, so a boot regression that drops the presence store in prod is unguardable by crate tests; guarded here is `here_recipients` logic + `PresenceStore` integration — and only while Redis is up.

### AT-5 — each suppression layer removes exactly its one expected recipient

Baseline: `@everyone` (or reply setup) that would notify {bob, carol, dave}; exactly one is suppressed; assert the other two rows exist and the suppressed one's row does not. `bundles=false` everywhere so replies land in `notifications`.

| # | Layer | Seed | Suppressed | Positive control |
|---|---|---|---|---|
| AT-5a | room mute | `insert_channel_mute(bob, room)` | bob | carol, dave present |
| AT-5b | DND | `set_dnd(bob, Some(0), Some(1440))` — correction D3, hole-free | bob | others present |
| AT-5c | snooze | `set_snooze(bob, Some(now + 10 min))` (`is_snoozed` = `now < until`, `:71`; margin kills boundary flake) | bob | others present |
| AT-5d | block | `BlockRepo::block(bob, alice)` (bob blocks sender; `blockers_of(sender)` `orig.rs:818–832`) | bob | others present |
| AT-5e | workspace mute | `WorkspaceMuteRepo::mute(bob, WorkspaceId::from_uuid(Uuid::nil()))` (legacy room is in nil workspace, V14; batch `muted_participants` `orig.rs:834–867`) | bob | others present |
| AT-5f | thread mute | root by alice; carol+dave `subscribe_authorized(root)` (`thread_subscription.rs:193`); bob replies → targets {alice, carol, dave} (`:676–683` + thread subscribers `:694–713`); `ThreadMuteRepo::mute_authorized(carol, root)` | carol | alice + dave present |
| AT-5g | thread level | same reply setup; `set_level_authorized(dave, root, "none")` (`thread_notification_prefs.rs:162`); `set_level_authorized(carol, root, "mentions")` | dave (always); carol when reply lacks @carol | alice present (`levels_for` default `"all"`, `orig.rs:900`); positive control: reply **mentioning** carol → carol present, dave still absent ("none" beats mention) |

The per-prefs batch filter is `should_deliver` (`notification_prefs.rs:96`), applied at `orig.rs:927–965`.

### AT-6 — relay loops

- **AT-6a outbox relay**: **self-seeds its own state** (review C1 — no longer borrows AT-1's pending row): send a fresh mention message, assert `pending_notify_outbox(msg.id)` is `Some` (this is AT-1's dropped assertion 4, re-covered here as the precondition), then `svc.dispatch_event_outbox_batch(10)` returns ≥ 1. Assert: `pending_for_message(msg.id)` is `None` (row published, `event_outbox.rs:447`); among `MockBus.published`, exactly **one** frame on `im.room.{room}` whose parsed JSON `event_id == delivery_id` (the send already published the Message frame on the same subject — filter by event_id, don't count total frames); that frame has a numeric `seq` stamp (outbox.rs:171 `assign_seq_if_absent` + LocalSeqProvider default, V15) and decodes to `NotifyBatch` with the same recipients (`outbox_wire_bytes` `outbox.rs:316`). The global batch may also publish other tests' pending rows — harmless: no other test asserts pending state and all `count_*` are message-scoped.
- **AT-6b side-effect relay**: seed a jobless message (`insert_message_row`), seed `insert_side_effect_job(msg, version=1, "notifications")`. Under the `BATCH_SERIAL` mutex (§5.6): `dispatch_message_side_effect_batch(10)` returns 1; job completed; `count_notifications(msg.id) == 1` for the sole member; second call returns 0. Negative control (optional): set `messages.deleted_at = now()` on a second seeded pair → batch completes the claim without rows (deleted branch `side_effects.rs:116–124`), no crash, `count_notifications == 0`.

### AT-7 — stale side-effect version completes without spending

- Setup: `bundles=false` service. `send_message` v1 → fast path completes v1 Embed/Moderate (1 `ai_jobs` row each — `complete_with_ai_job` `message_side_effect.rs:272`). `svc.edit_message(alice, msg.id, new_blocks, None)` → v2 (edit appends fresh Embed+Moderate at version 2, `message/events.rs`).
- **Poll to quiescence before re-arming** (review Q3 — replaces the old "robust either way" claim): after `edit_message`, poll `message_side_effect_jobs` until **every** row for `msg.id` has `completed_at IS NOT NULL` (bounded loop, e.g. 100 × 10 ms). Today the `#[cfg(test)]` await makes this immediate (V16); if the fast path is ever made fire-and-forget in tests, the re-arm would otherwise race the background dispatch — rearm clobbers `claimed_at`/`attempts` of a possibly-claimed row, the background's completion `WHERE attempts = $2` misses, `mark_failed` fires, and the delta goes nondeterministic 0/2/3. Quiescence is correct under both semantics.
- Act (correction D2): re-arm **three** rows — stale v1 Embed, plus v2 Embed and v2 Moderate — **each `rearm_side_effect_job` must return 1** (vacuity guard C3; 1/1/1), and assert `side_effect_job_state(...).attempts` incremented on each (the attempts flip proves each re-arm hit a live row, not a ghost). Then, under the `BATCH_SERIAL` mutex (§5.6), `svc.dispatch_message_side_effect_batch(10)` (returns 3).
- Assert:
  1. Stale v1 Embed job: `completed_at IS NOT NULL`, `last_error IS NULL` (guard branch `side_effects.rs:139–144` completes, never fails).
  2. `count_ai_jobs(msg.id)` delta == **exactly 2** (the v2 pair enqueued during this dispatch), **not 3** — the stale v1 job spent nothing. Intent pinned (Q3): the +2 asserts the *duplicate-insert* behavior of at-least-once redelivery — `ai_jobs` has no unique key on `(kind, target_id)` (mig 0002), so a re-dispatched v2 pair inserts two fresh rows; a future `ai_jobs` dedup would turn AT-7 red for the right reason.
  3. Positive control: the two fresh v2 jobs still enqueued normally (they are the source of the +2).

---

## §5 Compatibility constraints

1. **AGENTS.md §4.2 hard rules**: `cargo clippy --workspace --all-targets` must add zero warnings (follow existing test style — `db_tests.rs` carries `#![allow(clippy::unwrap_used)]`; submodules inherit module lint levels). No root-file changes. No new deps (`fred`/`sqlx`/`time`/`uuid` already declared). `#[ignore]`-gating keeps `cargo test --workspace --lib` hermetic. Tests live under `db_tests/` registered in `db_tests.rs`.
2. **No production-code touch**: `dispatch_notifications`, `notify_delivery_id`, both relays, all repos byte-identical. `orig.rs` stays `metrics`-free (V6) — observability is a separate direction.
3. **Test seams that must NOT be used** (they are `#[cfg(test)] pub(crate)` in aero-storage and unreachable): `NotificationPrefsRepo::mute/unmute`, `MessageSideEffectRepo::insert_in_tx`. Raw SQL replaces them (V7/V8). Conversely the **reachable** in-crate `pub(crate)` items (`notify_delivery_id`, `NotifyBatchKind`) are fair game for assertions.
4. **`MockBus.subscribe` is `unimplemented!`** (`test_util.rs`) — no test may call any API that subscribes; the relay paths use only `publish_bytes_idempotent` (default trait method → `publish`, `aero-bus/traits.rs:104`) which MockBus records.
5. **Fast-path determinism dependency**: tests assume the `#[cfg(test)]` await in `kick_message_side_effects` (V16). This is a load-bearing invariant for AT-1/AT-2/AT-3 preconditions; if it ever changes, tests must poll job state instead of relying on post-`send_message` completion.
6. **Fresh state per test**: unique emails (`unique_email`), fresh rooms/members per test; raw-SQL seeds scoped by message/room ids; `count_*` filters always include `message_id`. **Parallel-execution claim (reconciled with the serialized runbook — review C1/Q4)**: the original "parallel execution cannot interfere" claim was false for *global batch return values*. The corrected invariant, by layer:
   - **Message-scoped counts and message-scoped dispatch** (`dispatch_message_side_effects_for`) are interference-immune under default parallel tokio test threads (each with its own 2-conn pool): every `count_*` filters by `message_id`.
   - **Global side-effect batch**: exactly three tests create *globally-claimable due jobs* — AT-2 (re-arm), AT-6b (seeded job), AT-7 (re-armed trio). These serialize their **seed → dispatch → assert** critical sections on a shared `pub(crate) static BATCH_SERIAL: std::sync::Mutex<()>` declared in `db_tests.rs` (visible to both submodules; const-constructible; std mutex held across awaits is fine here — only these tests contend, no re-entrancy/deadlock). AT-6a's global *outbox* batch can only publish other tests' rows, and after the AT-1#4 drop no test asserts another test's pending state — so the outbox side needs no lock.
   - **Runbook belt-and-braces**: the ignored suite runs with `--test-threads=1` (§7 step 5 and the gate script `scripts/test-notification-fanout.sh`, repo convention test-integration.sh:163/236), so even a future test that forgets the lock cannot flake under the documented procedure.
   - **Why mutex, not the alternatives** (decision, justified): *state-based assertions* were rejected — the claiming test completes jobs asynchronously, so a post-dispatch state read can race the claimer's completion (flake), and exact batch returns (the relay contract AT-6/AT-7 are written against) would be forfeited. *Message-scoped dispatch for AT-6b* was rejected — `dispatch_message_side_effect_batch` is the production timer driver (AGENTS.md §2), and the scoped variant is already exercised by every send/edit fast path (AT-1/2/3/5/7), so switching would leave the global claimer (`claim_due` + `FOR UPDATE SKIP LOCKED` + attempts+1 RETURNING) with zero coverage. The mutex preserves both exactness and coverage, and makes §6's "tests dispatch serially" true by construction.
7. **DB-gated procedure** (§7): throwaway DB only, never the shared dev DB; migrate from a freshly built binary (migrations are compile-embedded — `aero-storage/db.rs` `sqlx::migrate!("../../migrations")`); build **before** migrate or new migrations silently no-op.
8. **Redis leg (required, review Q2)**: AT-4c skips **only** when `REDIS_URL` is unset; a set-but-unreachable Redis fails the test loudly (panic, §3.2). The gate (§7) always exports `REDIS_URL` and pre-flights Redis, so the presence leg is a **required acceptance leg** — a Postgres-only manual run passes the DB branches but must not be mistaken for the full suite.

---

## §6 Failure modes

### Test-infrastructure failure modes (each designed out)

| Mode | Trigger | Mitigation |
|---|---|---|
| DND boundary flake | `set_dnd(0,1439)` at 23:59 UTC | Correction D3: `(0, 1440)` is inside for all 1440 minutes |
| Snooze boundary flake | `set_snooze` ≈ dispatch time | 10-minute margin (`now + 10 min`); `is_snoozed` is `now < until` |
| Redis down / unset | AT-4c presence | `REDIS_URL` unset → skip note + `None` (manual ad-hoc only); `REDIS_URL` set but unreachable → **panic** (fail-loud, Q2); the gate pre-flights Redis so the leg is always live in CI |
| MockBus lock poisoning | assertion panic inside lock | `BusError::Nats` propagates → test fails loudly, no hang |
| Parallel test interference | shared tables | per-test participants/rooms/messages; all counts message-scoped |
| Global-batch cross-test claims | AT-2/AT-6b/AT-7 create globally-claimable *due* side-effect jobs; a concurrent `dispatch_message_side_effect_batch` steals them → wrong return values / mid-test completion | `BATCH_SERIAL` mutex serializes the seed→dispatch→assert sections of AT-2/AT-6b/AT-7 (§5.6); runbook `--test-threads=1` is belt-and-braces |
| Lease expiry mid-test | slow CI | claims are immediate (nudge sets `available_at = now`); `rearm` sets `available_at` 1 min in the past so the batch claimer picks the row up |
| Attempts mismatch on completion | re-armed row | `claim_matching` increments `attempts` and returns post-claim rows (`message_side_effect.rs:166–190`); the `(job.id, job.attempts)` source tuple always matches the completion `WHERE attempts = $2` |
| Superseded-claim conflict | two concurrent dispatchers | `insert_many_outboxed` returns `Ok(false)` → `Error::Conflict`; global-batch callers serialize on `BATCH_SERIAL` (§5.6), so the conflict branch is never hit — the tests assert the happy-path completion, not the conflict |

### Product regressions these tests guard (the reason for the suite)

1. **Silent over-delivery**: a broken mute/DND/snooze/block/level layer leaks room membership or spams — each AT-5 row asserts the suppressed recipient's absence.
2. **Silent under-delivery / fail-open drift**: AT-4b/AT-4c assert the documented fail-open contract (`@here` with no/empty/disjoint presence still reaches all members).
3. **Duplicate notifications on redelivery**: AT-2 asserts the ON CONFLICT (delivery_id, participant_id) + (event_id) dedup end-to-end through the real relay; if mig 0137/0165 indexes were ever dropped, AT-2/AT-3 fail.
4. **Stale-version AI spend**: AT-7 pins the version guard — a regression that removes it would enqueue `ai_jobs` for superseded text (budget + wrong-content embedding).
5. **Relay state-machine stalls**: AT-6 pins claim → complete/publish → mark_published; a stuck `processing` state (lease bug) or a row left pending would fail `pending_for_message == None` and the second-batch `returns 0`.
6. **Empty-target completion (new in review)**: a plain/unmentioned message — or a broadcast filtered to zero — must complete its notifications claim without inserting rows (`orig.rs:1006–1007`). A regression that parks the claim would leave the job retrying forever with zero visible output; AT-2b pins the branch.

---

## §7 Migration / deployment steps — runbook v2 (enforceable acceptance gate)

**No schema or production migration.** The "migration" is the DB-gated acceptance gate, per AGENTS.md §4.3. It is now **enforceable**: `scripts/test-notification-fanout.sh` (wired into `scripts/test-integration.sh` step 1, `make test-notification-fanout`, `make ci-full`, and the CI `integration` job) executes the whole procedure below and fails closed on every guardrail.

### One-shot (recommended)

```bash
bash scripts/test-notification-fanout.sh
```

Creates `aero_test_notif_<pid>` (guarded: `^aero_[A-Za-z0-9_]{1,58}$`, never pre-dropped, registered create/drop, EXIT/INT/TERM trap), builds **before** migrate, migrates with both URL pairs, runs the hermetic gate + the suite with `--locked --test-threads=1`, pre-flights Redis, fails on dirty env, drops the DB on every exit path.

### Manual procedure — exact commands

```bash
# 0. Preconditions (fail-closed; the gate script enforces these)
#    * Clean env: NO AERO_BLOCKED_WORDS / AERO_PII_GUARD / AERO_PII_GUARD_PHONE /
#      any AERO_AI_* — they flip KeywordModerator::from_env (orig.rs:292),
#      PiiDetector::from_env (messages.rs:255) and AI fail-open paths.
#    * Redis up — the presence leg (AT-4c) is REQUIRED, never optional.
unset AERO_BLOCKED_WORDS AERO_PII_GUARD AERO_PII_GUARD_PHONE
unset "${!AERO_AI_@}"          # bash: all AERO_AI_* variables

# 1. Build FIRST — migrations are compile-embedded (sqlx::migrate!("../../migrations")).
cargo build --workspace --locked

# 2. Throwaway DB (never the shared dev DB; name must match ^aero_[A-Za-z0-9_]{1,58}$).
createdb aero_test_notif_<run>
export DATABASE_URL=postgres://aero:aero@localhost/aero_test_notif_<run>
#    aero-cli reads Figment's AERO__ namespace — plain DATABASE_URL is never
#    consulted, so without this the migrate would hit the shared dev DB.
export AERO__DATABASE__URL="$DATABASE_URL"
export REDIS_URL=redis://localhost:6379        # presence leg REQUIRED
export AERO__REDIS__URL="$REDIS_URL"

# 3. Migrate (after build — ordering is load-bearing; `-p aero-cli` is WRONG —
#    that selects the aero-eng engineering bin; migrate lives in aero-server's
#    aero-cli bin).
cargo run --locked --bin aero-cli -- migrate

# 4. Hermetic gate (unchanged, must stay green — proves the new tests are ignored).
cargo test --workspace --lib --locked

# 5. The suite — its own throwaway DB, serial (the relay loops claim GLOBAL
#    state, so parallel tests race each other; the gate's DB is dedicated so
#    other crates' leftover rows can't pollute exact batch return values).
cargo test -p aero-im-core --lib --locked db_tests:: -- --ignored --test-threads=1

# 6. Standing gates (enforced by CI jobs check/clippy/gates + make gate).
cargo check --workspace --all-targets --locked && cargo clippy --workspace --all-targets --locked \
  && scripts/truth-check.sh && scripts/file-size-check.sh

# 7. Cleanup (WITH (FORCE); the gate script's trap does this automatically).
dropdb --force aero_test_notif_<run>
```

**Presence leg contract**: the suite's `try_presence_store` may self-skip **only** when `REDIS_URL` is unset (manual ad-hoc runs). When `REDIS_URL` is set (always true in the gate) but unreachable, the suite must **fail loudly** — a green run must never be indistinguishable from "AT-4c never ran". The gate additionally pre-flights Redis reachability before spending a build+migrate cycle.

**Migration dependencies exercised by the suite** (all pre-existing): 0001 (messages/rooms/participants), **0002** (`ai_jobs` — `target_id`, **no `message_id` column**; `count_ai_jobs` filters `target_id`), 0018 (`channel_mutes`, `dnd_settings`), 0036 (message history), 0088 (thread mutes), 0137 (`notifications.delivery_id` + partial unique index), 0143 (bundles base), 0157 (`messages.version`), **0162** (`event_outbox` + `event_id` UNIQUE — AT-2 redispatch), **0163** (`event_kind` column + CHECK — `count_notify_outbox` depends on it), 0165 (`message_side_effect_jobs` + `notification_bundles.delivery_id`), **0174** (`messages.delivery_ordinal` NOT NULL — `insert_message_row` satisfies it via the auto-assign BEFORE-INSERT trigger). The full-migrate-on-throwaway-DB flow covers all of them regardless.

---

## §8 Testable acceptance mapping

| # | Criterion (spec §6) | Deterministic assertion | Fails when |
|---|---|---|---|
| AT-1 | mention → 2 inbox rows (kind `mention`, actor = sender) + 1 `notify` outbox row with `event_id == notify_delivery_id(msg, Mention)` + matching payload | `count_notifications==2`, `list` rows, `count_notify_outbox==1`, `event_id` equality, payload decode `NotifyBatch{recipients:{bob,carol}}` (pending-state assertion dropped in review — C1; re-covered as AT-6a's self-seeded precondition) | self-exclusion broke; dedup id drifted; payload/recipients wrong; fast-path publish of Message missing |
| AT-2 | re-arm (rows_affected==1) + redispatch → counts unchanged, job completed | counts 2/1, rearm returned 1, job `completed_at=Some, last_error=None` | ON CONFLICT index dropped (surfaces via error→`last_error` channel, not counts — Q1); claim not completed; conflict error surfaced; rearm matched 0 rows (vacuous pass) |
| AT-2b | empty-target completion: plain text / everyone-suppressed → no rows, claim completed | 0 notifications + 0 outbox + 0 bundles; job `completed_at=Some, last_error=None`; scoped redispatch returns 0 | claim never completed (job parks/retries forever); spurious empty outbox row; completion error surfaced |
| AT-3 | bundle vs immediate routing + per-kind delivery_id | immediate: 1 row + 1 outbox; bundled: 0 + 1 bundle row w/ `thread_root`+`delivery_id`; mixed: 1 notification + 1 bundle + **1** outbox row whose `event_id` (mention tag) is distinct from the bundle's `delivery_id` (reply tag) | routing partition (`has_replies`) regressed; bundle completion wrong; reply/mention id collision; bundle insert wrongly writes an outbox row (C2) |
| AT-4 | `@everyone` = members−sender; `@here` fail-open; presence narrows | 4a {bob,carol,dave}; 4b same; 4c {bob} / all / all (Redis leg required; set-but-unreachable Redis fails loudly) | broadcast token parse; `here_recipients` logic + `PresenceStore` integration (production boot `with_presence` wiring is out of crate-test reach — Q2.3); fail-open branches |
| AT-5a–g | each layer removes exactly its recipient | suppressed row absent, two controls present; 5g mention positive control | mute/DND/snooze/block/workspace-mute/thread-mute/level logic regressed |
| AT-6 | outbox published + marked; side-effect claims complete (1 → 0) | self-seeded pending precondition; `pending_for_message==None`; exactly one `im.room.{room}` frame with `event_id==delivery_id` + numeric `seq`; batch returns 1 then 0 (under `BATCH_SERIAL`) | relay claim/complete/mark_published broken; seq stamp missing; wire bytes wrong |
| AT-7 | stale v1 completes without spending; fresh v2 enqueued | quiescence poll before re-arm; re-arms returned 1/1/1 + attempts flipped; v1 row `completed_at=Some, last_error=None`; `ai_jobs` delta == 2 not 3; batch returned 3 (under `BATCH_SERIAL`) | version guard removed; stale job enqueues; fresh jobs lost; re-arm raced the fast path (nondeterminism — Q3) |

Plus the standing gates: `cargo check --workspace` clean · `cargo clippy --workspace --all-targets` no new warnings · `cargo test --workspace --lib` green (hermetic) · `truth-check.sh` / `file-size-check.sh` 0 violations.

---

## §9 Open items / risks

1. **AT-4c Redis presence is a REQUIRED gate leg (runbook v2)**: the gate (`scripts/test-notification-fanout.sh` + CI) always exports `REDIS_URL`/`AERO__REDIS__URL` and pre-flights Redis, so the skip is never hit there. The helper contract: self-skip **only** when `REDIS_URL` is unset (manual ad-hoc runs); when set but unreachable, fail loudly. Open risk reduced to "manual runs without Redis silently narrow coverage" — documented in the helper's skip note. Residual (Q2.3): even with Redis up, a production boot regression that drops `with_presence` is unguardable by crate tests — the suite guards `here_recipients` + `PresenceStore`, not the boot wiring.
2. **`dispatch_notifications` direct-call alternative** remains available in-crate (`pub(crate)`) if a future test needs to bypass the claim machinery — not used by this design.
3. **User-group / keyword-alert fan-out** intentionally unasserted (spec §3 non-goal); their stores are not wired into `notification_service`, so those branches of `dispatch_notifications` are dead code in every test (Q4.2). Consequence: the suite guards the mention / reply / broadcast / suppression branches — it must **not** be described as guarding "the notification fan-out pipeline" unqualified. A metrics/refactor direction touching orig.rs could flip their fail-open with zero signal; wiring the two stores plus one smoke assertion each is the cheap future close.
4. **Bundle flush** (`flush_notification_bundles`) untested beyond observing bundle rows — the aggregation sweep is a separate timer path (AGENTS.md §2), out of scope. Accepted gap with mitigation (Q4.1): storage-level DB-gated tests already cover aggregation, delivery-id regeneration, departed recipients and idempotent re-flush (`notification_bundle.rs:587+`); still unguarded is the service-level flush→relay→MockBus wire and the multi-node concurrent-flush double-notify invariant (ON CONFLICT can't save you — the delivery id is regenerated per flush; only `FOR UPDATE SKIP LOCKED` does). Cheap future close: AT-3c — backdate the bundle's `created_at`, call `svc.flush_notification_bundles()` + `dispatch_event_outbox_batch(10)`, assert the notification, the NotifyBatch frame, and bundle consumption.
5. **AT-7 fast-path race — resolved by design (Q3)**: the old "robust either way" claim was false — a fire-and-forget edit fast-kick would let the re-arm race the background dispatch (rearm clobbers `claimed_at`/`attempts`; the background's `WHERE attempts = $2` completion misses → `mark_failed` → nondeterministic 0/2/3). AT-7 now polls `message_side_effect_jobs` to quiescence (all rows `completed_at NOT NULL`) after `edit_message` and before re-arming, which is correct under both the awaited and the fire-and-forget semantics. Residual: if the poll bound (100 × 10 ms) is ever too short for a slow CI, the re-arm's rows_affected==1 assertion fails loudly instead of silently skewing the delta.

---

## §10 Review reconciliation ledger (second pass)

Material findings from the three adversarial reviews, each encoded where indicated — no known contradiction remains between the design and the reports.

| # | Finding (source) | Encoded at |
|---|---|---|
| R1 | §7 migrate used `-p aero-cli` → selects package `aero-cli` whose bin is `aero-eng` (no migrate); migrate lives in package `aero-server`'s `[[bin]] aero-cli` (src/bin/aero-cli.rs:145) | §7 step 3 — `cargo run --locked --bin aero-cli -- migrate` + rationale comment |
| R2 | §7 exported only `DATABASE_URL`; `aero-cli migrate` reads config via Figment `AERO__DATABASE__URL` → would hit the shared dev DB | §7 step 2 — both exports (test-integration.sh:217–220 pattern) |
| R3 | Repo convention: `--locked` + `--test-threads=1` on ignored runs | §7 steps 1/4/5 + gate script |
| R4 | `count_ai_jobs`: `ai_jobs` has no `message_id` column — filter `target_id`, per message_side_effect.rs:538 | §3.2 helper + §7 migration list (0002) |
| R5 | C2: `insert_many_idempotent` writes no outbox row (only `flush()` does) — AT-3 mixed = 1 outbox row + 1 bundle row, ids distinct by tag | §4 AT-3, §8 AT-3 |
| R6 | C3: `rearm_side_effect_job` vacuously matching 0 rows lets AT-2/AT-7 pass — helpers return rows_affected, asserted 1 (1/1/1 AT-7) + attempts flip | §3.2, §4 AT-2/AT-7, §8 AT-2/AT-7 |
| R7 | C1: AT-1#4 pending-state assertion races AT-6a's global outbox batch; AT-6b↔AT-7 global side-effect batch race | §4 AT-1 (dropped), §4 AT-6a (self-seed), §5.6 (BATCH_SERIAL), §7 step 5 (`--test-threads=1`) |
| R8 | Q2: AT-4c skip masked dead Redis — skip only when `REDIS_URL` unset, fail loudly when set-but-unreachable; Redis leg required | §3.2, §4 AT-4c, §5.8, §6, §7 presence-leg contract, §8 AT-4, §9.1 |
| R9 | Q3: open item 5 "robust either way" false — fire-and-forget edit fast-kick races the re-arm | §4 AT-7 (poll-to-quiescence), §9.5 |
| R10 | Q4: empty-target completion branch unasserted (every plain message routes through orig.rs:1006–1007) | §4 AT-2b, §6 regression 6, §8 AT-2b |
| R11 | Q1: AT-2's index-drop failure fires via error→`last_error`, not counts | §4 AT-2, §8 AT-2 |
| R12 | Q2.3: "presence wiring" overstates coverage — boot `with_presence` wiring unguardable by crate tests | §4 AT-4c note, §8 AT-4, §9.1 |
| R13 | Migration-dependency list omitted 0162/0163/0174 (load-bearing for `insert_message_row` / `count_notify_outbox` / AT-2) | §7 migration list (0162/0163/0174 + 0002) |
| R14 | Clean env note: `AERO_BLOCKED_WORDS` / `AERO_PII_GUARD*` / `AERO_AI_*` must be unset (env-driven KeywordModerator/PiiDetector) | §7 step 0 (gate preconditions) |
| R15 | Q4.1: bundle-flush service-level wire unguarded (accepted; storage tests exist) | §9.4 (documented, optional AT-3c close) |
| R16 | Q4.2: user-group/keyword-alert branches dead in tests — coverage claim must be scoped | §9.3 |

**Reconciliation decision (parallelism)**: §5.6's original "parallel execution cannot interfere" claim was false for global batch return values. Chosen mechanism: **shared `BATCH_SERIAL` mutex** serializing the seed→dispatch→assert sections of the three tests that create globally-claimable due jobs (AT-2, AT-6b, AT-7), plus dropping AT-1#4 in favor of AT-6a's self-seeded precondition, plus `--test-threads=1` in the runbook/gate as belt-and-braces. *State-based assertions* were rejected: the claiming test completes jobs asynchronously, so a post-dispatch state read can race the claimer's completion (flake), and exact batch returns — the relay contract AT-6/AT-7 are written against — would be forfeited. *Message-scoped dispatch for AT-6b* was rejected: `dispatch_message_side_effect_batch` is the production timer driver (AGENTS.md §2), and the scoped variant is already exercised by every send/edit fast path, so switching would leave the global claimer (`claim_due` + `FOR UPDATE SKIP LOCKED` + attempts+1 RETURNING) with zero coverage. The mutex preserves both exactness and coverage and makes §6's "tests dispatch serially" true by construction; the runbook serialization additionally protects future tests that forget the lock.
